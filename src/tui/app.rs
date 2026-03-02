#![allow(dead_code)]

use crate::core::shell::sanitize;
use crate::core::state::WorkerPhase;
use crate::core::{agent::AgentKind, git, ipc, merge, socket_listener, state, tmux};
use crate::swarm_log;
use chrono::{DateTime, Local};
use color_eyre::Result;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;
use tokio::sync::mpsc;

/// Pane foreground colors for selected/dimmed states.
const PANE_FG_SELECTED: &str = "#dcdce1"; // full FROST brightness
const PANE_FG_DIMMED: &str = "#5a5550"; // readable gray, not invisible
const PANE_BG_SELECTED: &str = "#1e1b18"; // deepest dark — max contrast for focused pane
const PANE_BG_DIMMED: &str = "#302c26"; // warm gray wash — "frosted glass" for unfocused

/// Hex colors for worktree pane border titles.
const WORKTREE_BORDER_COLORS: &[&str] = &[
    "#b47a3c", // warm brown
    "#3c7ab4", // cool blue
    "#3cb43c", // forest green
    "#8c3cb4", // purple
    "#3cb4b4", // teal
    "#b4963c", // amber
    "#b43c78", // rose
    "#64b43c", // olive
];

/// Status of a tracked tmux pane.
#[derive(Debug, Clone, PartialEq)]
pub enum PaneStatus {
    Running,
    Done,
}

/// A tracked tmux pane (agent or terminal).
#[derive(Debug, Clone)]
pub struct TrackedPane {
    pub pane_id: String,
    pub status: PaneStatus,
}

/// PR info fetched from `gh`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PrInfo {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub url: String,
}

impl PrInfo {
    /// Returns true when this PR just transitioned to MERGED relative to `prev`.
    /// Used to decide whether to auto-close the worktree.
    pub fn is_newly_merged(&self, prev: Option<&PrInfo>) -> bool {
        self.state == "MERGED" && prev.is_none_or(|p| p.state != "MERGED")
    }
}

/// A worktree — the primary unit of work.
#[derive(Debug, Clone)]
pub struct Worktree {
    pub id: String,
    pub branch: String,
    pub prompt: String,
    pub agent_kind: AgentKind,
    pub repo_path: PathBuf,
    pub worktree_path: PathBuf,
    pub created_at: DateTime<Local>,
    pub agent: Option<TrackedPane>,
    pub terminals: Vec<TrackedPane>,
    pub pr: Option<PrInfo>,
    /// Worker lifecycle phase — single source of truth for worker state.
    pub phase: WorkerPhase,
    /// LLM-generated short summary of the task prompt.
    pub summary: Option<String>,
    /// Agent session status read from `.swarm/agent-status/<id>` (e.g. "waiting", "running").
    pub agent_session_status: Option<String>,
}

impl Worktree {
    /// Convert to persistable state.
    fn to_state(&self) -> state::WorktreeState {
        // Compute backward-compat status from phase
        let status = if self.phase.is_active() {
            "running".to_string()
        } else {
            "done".to_string()
        };

        state::WorktreeState {
            id: self.id.clone(),
            branch: self.branch.clone(),
            prompt: self.prompt.clone(),
            agent_kind: self.agent_kind.clone(),
            repo_path: self.repo_path.clone(),
            worktree_path: self.worktree_path.clone(),
            created_at: self.created_at,
            agent: self
                .agent
                .as_ref()
                .map(|p| state::PaneState::new(p.pane_id.clone())),
            terminals: self
                .terminals
                .iter()
                .map(|p| state::PaneState::new(p.pane_id.clone()))
                .collect(),
            summary: self.summary.clone(),
            pr: self.pr.clone(),
            phase: self.phase.clone(),
            status,
            agent_session_status: self.agent_session_status.clone(),
        }
    }

    /// Restore from persisted state.
    fn from_state(ws: &state::WorktreeState) -> Self {
        Self {
            id: ws.id.clone(),
            branch: ws.branch.clone(),
            prompt: ws.prompt.clone(),
            agent_kind: ws.agent_kind.clone(),
            repo_path: ws.repo_path.clone(),
            worktree_path: ws.worktree_path.clone(),
            created_at: ws.created_at,
            agent: ws.agent.as_ref().map(|p| TrackedPane {
                pane_id: p.pane_id.clone(),
                status: PaneStatus::Running,
            }),
            terminals: ws
                .terminals
                .iter()
                .map(|p| TrackedPane {
                    pane_id: p.pane_id.clone(),
                    status: PaneStatus::Running,
                })
                .collect(),
            pr: ws.pr.clone(),
            phase: ws.phase.clone(),
            summary: ws.summary.clone(),

            agent_session_status: None,
        }
    }

    /// Check if this worktree has a running agent or terminal.
    pub fn is_alive(&self) -> bool {
        let agent_alive = self
            .agent
            .as_ref()
            .is_some_and(|p| p.status == PaneStatus::Running);
        let term_alive = self
            .terminals
            .iter()
            .any(|p| p.status == PaneStatus::Running);
        agent_alive || term_alive
    }

    /// Short indicator showing what panes exist.
    pub fn window_indicator(&self) -> String {
        let a = if self.agent.is_some() { "A" } else { "" };
        let t_count = self.terminals.len();
        match (a.is_empty(), t_count) {
            (true, 0) => "[-]".to_string(),
            (false, 0) => "[A]".to_string(),
            (true, 1) => "[T]".to_string(),
            (true, n) => format!("[T{}]", n),
            (false, 1) => "[A+T]".to_string(),
            (false, n) => format!("[A+T{}]", n),
        }
    }

    /// Overall status — running if any pane is running.
    pub fn status(&self) -> PaneStatus {
        if self.is_alive() {
            PaneStatus::Running
        } else {
            PaneStatus::Done
        }
    }
}

/// TUI interaction modes.
#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Normal,
    Input,
    RepoSelect,
    AgentSelect,
    Confirm,
    Help,
}

/// Pending action that needs confirmation.
#[derive(Debug, Clone)]
pub enum PendingAction {
    Merge(usize),
    Close(usize),
}

/// Main application state.
pub struct App {
    pub work_dir: PathBuf,
    pub repos: Vec<PathBuf>,
    pub default_agent: AgentKind,
    pub session_name: String,
    pub worktrees: Vec<Worktree>,
    pub selected: usize,
    pub mode: Mode,
    pub input_buffer: String,
    pub input_cursor: usize,
    pub input_label: String,
    pub agent_select_index: usize,
    pub repo_select_index: usize,
    pub pending_action: Option<PendingAction>,
    pub confirm_message: String,
    pub status_message: Option<(String, Instant)>,
    pub show_help: bool,
    pub tick_count: u64,
    pub sidebar_pane_id: Option<String>,
    /// Scroll offset (in lines) for the sidebar worktree list.
    pub list_scroll: Cell<usize>,
    prev_selected: Option<usize>,
    layout_dirty: bool,
    /// When true, pane border styles need updating on the next tick.
    pane_style_dirty: bool,
    last_refresh: Instant,
    last_pr_check: Instant,
    last_inbox_check: Instant,
    last_inbox_pos: u64,
    summary_tx: mpsc::UnboundedSender<(String, String)>,
    summary_rx: mpsc::UnboundedReceiver<(String, String)>,
    inbox_rx: mpsc::UnboundedReceiver<ipc::InboxMessage>,
    _socket_handle: Option<socket_listener::SocketListenerHandle>,
    /// When true, suppress event emission during relaunch_dead_agents
    /// to avoid false AgentSpawned notifications on swarm restart.
    pub is_startup_relaunch: bool,
    /// Worktree IDs relaunched during startup (unused now, kept for compatibility).
    startup_relaunched: std::collections::HashSet<String>,
}

impl App {
    pub fn new(work_dir: PathBuf, agent: String) -> Result<Self> {
        let repos = git::detect_repos(&work_dir)?;
        let default_agent = AgentKind::from_str(&agent).unwrap_or(AgentKind::ClaudeTui);

        // Derive session name from dir
        let dir_name = work_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "swarm".to_string());
        let session_name = format!("swarm-{}", dir_name);

        let (summary_tx, summary_rx) = mpsc::unbounded_channel();

        // Initialise file logger (before anything that might log)
        crate::core::log::init(&work_dir);

        // Start socket listener (before restore so we're ready for messages)
        let (inbox_rx, socket_handle) = match socket_listener::start(&work_dir) {
            Ok((rx, handle)) => (rx, Some(handle)),
            Err(e) => {
                swarm_log!("[swarm] failed to start socket listener: {}", e);
                let (_tx, rx) = mpsc::unbounded_channel();
                (rx, None)
            }
        };

        let mut app = Self {
            work_dir,
            repos,
            default_agent,
            session_name,
            worktrees: Vec::new(),
            selected: 0,
            mode: Mode::Normal,
            input_buffer: String::new(),
            input_cursor: 0,
            input_label: String::new(),
            agent_select_index: 0,
            repo_select_index: 0,
            pending_action: None,
            confirm_message: String::new(),
            status_message: None,
            show_help: false,
            tick_count: 0,
            sidebar_pane_id: None,
            list_scroll: Cell::new(0),
            prev_selected: None,
            layout_dirty: false,
            pane_style_dirty: false,
            last_refresh: Instant::now(),
            last_pr_check: Instant::now(),
            last_inbox_check: Instant::now(),
            last_inbox_pos: 0,
            summary_tx,
            summary_rx,
            inbox_rx,
            _socket_handle: socket_handle,
            is_startup_relaunch: false,
            startup_relaunched: std::collections::HashSet::new(),
        };

        // Restore previous session
        app.restore_state();

        // Drain any pending file inbox messages that arrived before socket was ready
        app.drain_file_inbox();

        Ok(app)
    }

    // ── State Persistence ──────────────────────────────────

    fn restore_state(&mut self) {
        // Phase 1: restore from state file
        let mut restored_inbox_pos = false;
        if let Ok(Some(saved)) = state::load_state(&self.work_dir) {
            // Always restore inbox position so we never replay old messages,
            // even if the tmux session is gone.
            if saved.last_inbox_pos > 0 {
                self.last_inbox_pos = saved.last_inbox_pos;
                restored_inbox_pos = true;
            }

            if tmux::session_exists(&saved.session_name) {
                self.session_name = saved.session_name;
                self.sidebar_pane_id = saved.sidebar_pane_id;

                // Get all live pane IDs in the session
                let session_window = self.session_name.clone();
                let live_pane_ids: Vec<String> = tmux::list_panes(&session_window)
                    .unwrap_or_default()
                    .iter()
                    .map(|p| p.pane_id.clone())
                    .collect();

                for ws in &saved.worktrees {
                    let mut wt = Worktree::from_state(ws);

                    let agent_alive = wt
                        .agent
                        .as_ref()
                        .is_some_and(|a| live_pane_ids.contains(&a.pane_id));

                    if let Some(ref mut agent) = wt.agent
                        && !live_pane_ids.contains(&agent.pane_id)
                    {
                        agent.status = PaneStatus::Done;
                    }
                    for term in &mut wt.terminals {
                        if !live_pane_ids.contains(&term.pane_id) {
                            term.status = PaneStatus::Done;
                        }
                    }

                    // Restart recovery: fix phase based on pane liveness
                    match wt.phase {
                        WorkerPhase::Creating | WorkerPhase::Starting if !agent_alive => {
                            // Agent pane died during startup — mark as Completed
                            // (not Failed) so relaunch_dead_agents can restart it.
                            wt.phase = WorkerPhase::Completed;
                        }
                        WorkerPhase::Creating | WorkerPhase::Starting if agent_alive => {
                            // Pane is alive but startup incomplete — will transition
                            // to Running when prompt delivers
                            wt.phase = WorkerPhase::Starting;
                        }
                        WorkerPhase::Running | WorkerPhase::Waiting if !agent_alive => {
                            wt.phase = WorkerPhase::Completed;
                        }
                        _ => {}
                    }

                    self.worktrees.push(wt);
                }
            }
        }

        // If we didn't restore an inbox position (fresh start or old state format),
        // skip to the end of the inbox so we don't replay historical messages.
        if !restored_inbox_pos {
            let inbox_path = self.work_dir.join(".swarm").join("inbox.jsonl");
            if let Ok(meta) = std::fs::metadata(&inbox_path) {
                self.last_inbox_pos = meta.len();
            }
        }

        // Note: we intentionally do NOT truncate the inbox on startup.
        // Position tracking (last_inbox_pos) already skips processed messages,
        // and truncating creates a race condition when messages are written
        // between session creation and TUI initialization.

        // Phase 2: discover orphaned worktrees from git (scan all repos)
        let repos_to_scan: Vec<PathBuf> = if self.repos.is_empty() {
            vec![self.work_dir.clone()]
        } else {
            self.repos.clone()
        };
        let wt_base = self.worktree_base();
        let mut orphan_count = 0;

        for repo_path in &repos_to_scan {
            if let Ok(git_worktrees) = git::list_worktrees(repo_path) {
                let tracked_paths: Vec<PathBuf> = self
                    .worktrees
                    .iter()
                    .map(|w| w.worktree_path.clone())
                    .collect();

                for (wt_path, branch) in &git_worktrees {
                    if !branch.starts_with("swarm/") {
                        continue;
                    }
                    if !wt_path.starts_with(&wt_base) {
                        continue;
                    }
                    if tracked_paths.contains(wt_path) {
                        continue;
                    }

                    let id = wt_path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| branch.clone());

                    self.worktrees.push(Worktree {
                        id,
                        branch: branch.clone(),
                        prompt: String::new(),
                        agent_kind: self.default_agent.clone(),
                        repo_path: repo_path.clone(),
                        worktree_path: wt_path.clone(),
                        created_at: Local::now(),
                        agent: None,
                        terminals: Vec::new(),
                        pr: None,
                        phase: WorkerPhase::Completed,
                        summary: None,
            
                        agent_session_status: None,
                    });
                    orphan_count += 1;
                }
            }
        }

        if orphan_count > 0 {
            self.save_state();
        }

        let total = self.worktrees.len();
        if total > 0 {
            // Full update: re-apply colors and selection to all live panes
            self.prev_selected = None;
            self.update_pane_selection();

            // Request summaries for worktrees that don't have one yet
            for wt in &self.worktrees {
                if wt.summary.is_none() && !wt.prompt.is_empty() {
                    self.request_summary(wt.id.clone(), wt.prompt.clone());
                }
            }

            self.flash(format!(
                "restored {} worktree{}",
                total,
                if total == 1 { "" } else { "s" }
            ));
        }
    }

    pub fn save_state(&self) {
        if let Err(e) = self.save_state_inner() {
            swarm_log!("[swarm] save_state failed: {}", e);
            // Flash is not available from &self, but the error is logged
        }
    }

    fn save_state_inner(&self) -> Result<()> {
        let mut worktree_states: Vec<state::WorktreeState> =
            self.worktrees.iter().map(|w| w.to_state()).collect();

        // Read agent session status from .swarm/agent-status/<id> files
        let status_dir = self.work_dir.join(".swarm").join("agent-status");
        for ws in &mut worktree_states {
            if let Ok(contents) = std::fs::read_to_string(status_dir.join(&ws.id)) {
                let trimmed = contents.trim();
                if !trimmed.is_empty() {
                    ws.agent_session_status = Some(trimmed.to_string());
                }
            }
        }

        let swarm_state = state::SwarmState {
            session_name: self.session_name.clone(),
            sidebar_pane_id: self.sidebar_pane_id.clone(),
            worktrees: worktree_states,
            last_inbox_pos: self.last_inbox_pos,
        };

        state::save_state(&self.work_dir, &swarm_state)
    }

    /// Transition a worktree to a new phase with validation, logging, and event emission.
    fn transition(&mut self, idx: usize, to: WorkerPhase, reason: Option<&str>) {
        let wt = &self.worktrees[idx];
        let from = wt.phase.clone();

        if from == to {
            return; // no-op
        }

        if !from.can_transition_to(&to) {
            swarm_log!(
                "[swarm] illegal transition for {}: {} -> {}",
                wt.id, from, to
            );
            return;
        }

        let worktree_id = wt.id.clone();
        let now = Local::now();

        // Log to transitions.jsonl
        let _ = ipc::log_transition(
            &self.work_dir,
            &ipc::TransitionEntry {
                worktree: worktree_id.clone(),
                from: from.clone(),
                to: to.clone(),
                timestamp: now,
                reason: reason.map(|s| s.to_string()),
            },
        );

        // Emit PhaseChanged event to events.jsonl (skip during startup relaunch
        // and for deferred Starting→Running transitions of startup-relaunched workers
        // to avoid false AgentSpawned notifications in hive daemon)
        let suppress = self.is_startup_relaunch || self.startup_relaunched.remove(&worktree_id);
        if !suppress {
            let _ = ipc::emit_event(
                &self.work_dir,
                &ipc::SwarmEvent::PhaseChanged {
                    worktree: worktree_id.clone(),
                    from: from.clone(),
                    to: to.clone(),
                    timestamp: now,
                },
            );
        }

        swarm_log!("[swarm] {} : {} -> {}", worktree_id, from, to);

        // Update phase
        self.worktrees[idx].phase = to;
        self.save_state();
    }

    // ── Navigation ─────────────────────────────────────────

    pub fn select_next(&mut self) {
        if self.worktrees.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.worktrees.len();
        self.pane_style_dirty = true;
    }

    pub fn select_prev(&mut self) {
        if self.worktrees.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.worktrees.len() - 1
        } else {
            self.selected - 1
        };
        self.pane_style_dirty = true;
    }

    // ── Jump to Agent Pane ─────────────────────────────────

    pub fn jump_to_selected(&mut self) {
        if self.worktrees.is_empty() {
            return;
        }
        let idx = self.selected;
        let wt = &self.worktrees[idx];

        // If there's a live agent pane, jump to it
        if let Some(ref agent) = wt.agent
            && agent.status == PaneStatus::Running
        {
            let _ = tmux::select_pane(&agent.pane_id);
            return;
        }
        // If there's a live terminal pane, jump to it
        if let Some(term) = wt
            .terminals
            .iter()
            .find(|t| t.status == PaneStatus::Running)
        {
            let _ = tmux::select_pane(&term.pane_id);
            return;
        }

        // No live panes — open a shell in the worktree directory
        if let Err(e) = self.attach_terminal(idx) {
            self.flash(format!("error: {}", e));
        }
    }

    /// Relaunch an agent for a worktree that has no live panes.
    fn relaunch_agent(&mut self, idx: usize) -> Result<()> {
        let wt = &self.worktrees[idx];
        let dir = if wt.worktree_path.exists() {
            wt.worktree_path.clone()
        } else {
            wt.repo_path.clone()
        };
        let agent = wt.agent_kind.clone();
        let prompt = wt.prompt.clone();
        let wt_id = wt.id.clone();

        // Build launch command (runs directly in tmux pane, no shell)
        let cmd = if agent == AgentKind::ClaudeTui {
            use crate::core::shell::shell_quote;
            let exe = std::env::current_exe()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "swarm".to_string());
            let mut base = format!(
                "'{}' -d {} agent-tui --dangerously-skip-permissions --worktree-id {}",
                exe,
                shell_quote(&self.work_dir.to_string_lossy()),
                shell_quote(&wt_id),
            );
            if !prompt.is_empty() {
                let prompt_file = format!("/tmp/swarm-prompt-{}.txt", wt_id);
                let _ = std::fs::write(&prompt_file, &prompt);
                base.push_str(&format!(" --prompt-file {}", shell_quote(&prompt_file)));
            }
            base
        } else {
            agent.launch_cmd_with_prompt(&prompt, true)
        };

        // Smart split: horizontal if no live agents, vertical if stacking.
        // Pass cmd directly so tmux runs it (no shell prompt detection needed).
        let has_live_agents = self.worktrees.iter().enumerate().any(|(i, w)| {
            i != idx
                && w.agent
                    .as_ref()
                    .is_some_and(|a| a.status == PaneStatus::Running)
        });

        let dir_str = dir.to_string_lossy();
        let pane_id = if !has_live_agents {
            let split_from = self
                .sidebar_pane_id
                .clone()
                .unwrap_or_else(|| "%0".to_string());
            tmux::split_pane_horizontal_with_cmd(&split_from, &dir_str, 70, &cmd)?
        } else {
            let last_agent_pane = self
                .worktrees
                .iter()
                .enumerate()
                .rev()
                .filter(|(i, _)| *i != idx)
                .filter_map(|(_, w)| {
                    w.agent
                        .as_ref()
                        .filter(|a| a.status == PaneStatus::Running)
                        .map(|a| a.pane_id.clone())
                })
                .next()
                .unwrap_or_else(|| {
                    self.sidebar_pane_id
                        .clone()
                        .unwrap_or_else(|| "%0".to_string())
                });
            tmux::split_pane_vertical_with_cmd(&last_agent_pane, &dir_str, &cmd)?
        };

        // Set pane title to truncated prompt so it matches the sidebar
        let pane_title = if prompt.len() > 60 {
            format!(
                "{}…",
                &prompt[..prompt
                    .char_indices()
                    .take_while(|&(i, _)| i < 60)
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(60)]
            )
        } else {
            prompt.clone()
        };
        let _ = tmux::set_pane_title(&pane_id, &pane_title);

        self.worktrees[idx].agent = Some(TrackedPane {
            pane_id: pane_id.clone(),
            status: PaneStatus::Running,
        });

        // Transition directly to Running (command already running in pane)
        self.transition(idx, WorkerPhase::Starting, Some("agent relaunched"));
        self.transition(idx, WorkerPhase::Running, Some("command launched"));

        // Rebalance, re-apply styling, re-select sidebar (after setting agent so pane is included)
        self.rebalance_layout();
        let _ = tmux::apply_session_style(&self.session_name);
        if let Some(ref sidebar) = self.sidebar_pane_id {
            let _ = tmux::select_pane(sidebar);
        }
        self.prev_selected = None; // Force full update for new pane
        self.update_pane_selection();
        self.save_state();

        // Emit event (skip during startup relaunch to avoid false notifications)
        if !self.is_startup_relaunch {
            let _ = ipc::emit_event(
                &self.work_dir,
                &ipc::SwarmEvent::AgentStarted {
                    worktree: wt_id,
                    pane_id,
                    timestamp: Local::now(),
                },
            );
        }

        self.flash(format!("{} relaunched", agent.label()));
        Ok(())
    }

    /// Relaunch agents for worktrees that were restored with dead panes.
    pub fn relaunch_dead_agents(&mut self) {
        let indices: Vec<usize> = self
            .worktrees
            .iter()
            .enumerate()
            .filter(|(_, wt)| {
                let agent_dead = match &wt.agent {
                    None => true,
                    Some(a) => a.status == PaneStatus::Done,
                };
                agent_dead && wt.worktree_path.exists()
            })
            .map(|(i, _)| i)
            .collect();

        for idx in indices {
            if let Err(e) = self.relaunch_agent(idx) {
                swarm_log!("[swarm] failed to relaunch {}: {e}", self.worktrees[idx].id);
            }
        }
    }

    // ── New Worktree Flow ─────────────────────────────────

    pub fn start_new_worktree(&mut self) {
        let exe = std::env::current_exe()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "swarm".to_string());
        let dir_str = self.work_dir.to_string_lossy();
        let cmd = format!("'{}' -d '{}' pick", exe, dir_str);

        if let Err(e) = tmux::display_popup(&self.session_name, "60%", "50%", " new task ", &cmd) {
            self.flash(format!("error: {}", e));
        }
    }

    pub fn show_pr_url(&mut self) {
        if self.worktrees.is_empty() {
            self.flash("no worktree selected".to_string());
            return;
        }
        let wt = &self.worktrees[self.selected];
        if let Some(ref pr) = wt.pr {
            let exe = std::env::current_exe()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "swarm".to_string());
            let cmd = format!(
                "'{}' pr-popup --number {} --title {} --state {} --url {}",
                exe,
                pr.number,
                crate::core::shell::shell_quote(&pr.title),
                crate::core::shell::shell_quote(&pr.state),
                crate::core::shell::shell_quote(&pr.url),
            );

            if let Err(e) = tmux::display_popup(
                &self.session_name,
                "60%",
                "40%",
                &format!(" PR #{} ", pr.number),
                &cmd,
            ) {
                self.flash(format!("error: {}", e));
            }
        } else {
            self.flash("no PR found for this worktree".to_string());
        }
    }

    pub fn add_terminal_to_selected(&mut self) {
        if self.worktrees.is_empty() {
            self.flash("no worktree selected".to_string());
            return;
        }

        if let Err(e) = self.attach_terminal(self.selected) {
            self.flash(format!("error: {}", e));
        }
    }

    fn attach_terminal(&mut self, idx: usize) -> Result<()> {
        let wt = &self.worktrees[idx];
        let dir = if wt.worktree_path.exists() {
            wt.worktree_path.clone()
        } else {
            wt.repo_path.clone()
        };

        // Split from: live agent pane > live terminal > any live pane > sidebar
        let split_from = wt
            .agent
            .as_ref()
            .filter(|a| a.status == PaneStatus::Running)
            .map(|a| a.pane_id.clone())
            .or_else(|| {
                wt.terminals
                    .iter()
                    .find(|t| t.status == PaneStatus::Running)
                    .map(|t| t.pane_id.clone())
            })
            .or_else(|| {
                // Fall back to any live agent pane in the session
                self.worktrees.iter().find_map(|w| {
                    w.agent
                        .as_ref()
                        .filter(|a| a.status == PaneStatus::Running)
                        .map(|a| a.pane_id.clone())
                })
            })
            .or_else(|| self.sidebar_pane_id.clone())
            .unwrap_or_else(|| "%0".to_string());

        // If splitting from sidebar (no other panes), go horizontal; otherwise vertical
        let is_from_sidebar = self
            .sidebar_pane_id
            .as_ref()
            .is_some_and(|s| s == &split_from);

        let pane_id = if is_from_sidebar {
            tmux::split_pane_horizontal(&split_from, &dir.to_string_lossy(), 70)?
        } else {
            tmux::split_pane_vertical(&split_from, &dir.to_string_lossy())?
        };

        let term_num = wt.terminals.len() + 1;
        let title = format!("{}-t{}", wt.id, term_num);
        let _ = tmux::set_pane_title(&pane_id, &title);

        self.worktrees[idx].terminals.push(TrackedPane {
            pane_id: pane_id.clone(),
            status: PaneStatus::Running,
        });

        // Rebalance layout, re-apply styling, re-select sidebar
        self.rebalance_layout();
        let _ = tmux::apply_session_style(&self.session_name);
        if let Some(ref sidebar) = self.sidebar_pane_id {
            let _ = tmux::select_pane(sidebar);
        }

        self.prev_selected = None; // Force full update for new pane
        self.update_pane_selection();
        self.save_state();
        self.flash(format!("terminal {} attached", term_num));
        Ok(())
    }

    // ── Input Handling ─────────────────────────────────────

    pub fn input_char(&mut self, c: char) {
        self.input_buffer.insert(self.input_cursor, c);
        self.input_cursor += 1;
    }

    pub fn input_backspace(&mut self) {
        if self.input_cursor > 0 {
            self.input_cursor -= 1;
            self.input_buffer.remove(self.input_cursor);
        }
    }

    pub fn input_cursor_left(&mut self) {
        if self.input_cursor > 0 {
            self.input_cursor -= 1;
        }
    }

    pub fn input_cursor_right(&mut self) {
        if self.input_cursor < self.input_buffer.len() {
            self.input_cursor += 1;
        }
    }

    pub fn cancel_input(&mut self) {
        self.mode = Mode::Normal;
        self.input_buffer.clear();
        self.input_cursor = 0;
        self.pending_action = None;
    }

    pub async fn submit_input(&mut self) {
        let prompt = self.input_buffer.clone();
        if prompt.trim().is_empty() {
            self.cancel_input();
            return;
        }

        self.mode = Mode::AgentSelect;
        self.agent_select_index = 0;
    }

    // ── Repo Selection ─────────────────────────────────────

    pub fn repo_select_next(&mut self) {
        let count = self.repos.len();
        if count > 0 {
            self.repo_select_index = (self.repo_select_index + 1) % count;
        }
    }

    pub fn repo_select_prev(&mut self) {
        let count = self.repos.len();
        if count > 0 {
            self.repo_select_index = if self.repo_select_index == 0 {
                count - 1
            } else {
                self.repo_select_index - 1
            };
        }
    }

    pub fn confirm_repo(&mut self) {
        self.mode = Mode::Input;
        self.input_buffer.clear();
        self.input_cursor = 0;
        self.input_label = "task".to_string();
    }

    pub async fn select_repo_by_index(&mut self, idx: usize) {
        if idx < self.repos.len() {
            self.repo_select_index = idx;
            self.confirm_repo();
        }
    }

    // ── Agent Selection ────────────────────────────────────

    pub fn agent_select_next(&mut self) {
        let count = AgentKind::all().len();
        self.agent_select_index = (self.agent_select_index + 1) % count;
    }

    pub fn agent_select_prev(&mut self) {
        let count = AgentKind::all().len();
        self.agent_select_index = if self.agent_select_index == 0 {
            count - 1
        } else {
            self.agent_select_index - 1
        };
    }

    pub async fn select_agent_by_index(&mut self, idx: usize) {
        let agents = AgentKind::all();
        if idx < agents.len() {
            self.agent_select_index = idx;
            self.confirm_agent().await;
        }
    }

    pub async fn confirm_agent(&mut self) {
        let agents = AgentKind::all();
        let agent = agents[self.agent_select_index].clone();
        let prompt = self.input_buffer.clone();
        let repo_path = self
            .repos
            .get(self.repo_select_index)
            .cloned()
            .unwrap_or_else(|| self.work_dir.clone());

        self.mode = Mode::Normal;
        self.input_buffer.clear();
        self.input_cursor = 0;

        if let Err(e) = self.create_worktree_with_agent(&prompt, agent, &repo_path, None) {
            self.flash(format!("error: {}", e));
        }
    }

    fn create_worktree_with_agent(
        &mut self,
        prompt: &str,
        agent: AgentKind,
        repo_path: &std::path::Path,
        start_point: Option<&str>,
    ) -> Result<()> {
        self.ensure_session()?;

        let repo_path = repo_path.to_path_buf();
        let repo_name = git::repo_name(&repo_path);
        let sanitized = sanitize(prompt);

        let _ = git::prune_worktrees(&repo_path);

        let mut num = self.next_worktree_num();
        loop {
            let candidate_dir = self.worktree_base().join(format!("{}-{}", sanitized, num));
            let candidate_branch = format!("swarm/{}-{}", sanitized, num);
            if !candidate_dir.exists() && !git::branch_in_worktree(&repo_path, &candidate_branch) {
                break;
            }
            num += 1;
        }

        let branch_name = format!("swarm/{}-{}", sanitized, num);
        let wt_dir_name = format!("{}-{}", sanitized, num);
        let worktree_dir = self.worktree_base().join(&wt_dir_name);
        let window_name = format!("{}-{}", repo_name, num);

        if let Some(parent) = worktree_dir.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Phase 1: Push with Creating phase (no agent yet)
        self.worktrees.push(Worktree {
            id: window_name.clone(),
            branch: branch_name.clone(),
            prompt: prompt.to_string(),
            agent_kind: agent.clone(),
            repo_path: repo_path.clone(),
            worktree_path: worktree_dir.clone(),
            created_at: Local::now(),
            agent: None,
            terminals: Vec::new(),
            pr: None,
            phase: WorkerPhase::Creating,
            summary: None,

            agent_session_status: None,
        });
        let idx = self.worktrees.len() - 1;
        self.save_state();

        // Phase 2: Create git worktree
        if let Err(e) = git::create_worktree(&repo_path, &branch_name, &worktree_dir, start_point)
        {
            swarm_log!(
                "[swarm] git worktree creation failed for {}: {}",
                window_name, e
            );
            self.worktrees.remove(idx);
            self.save_state();
            return Err(e);
        }

        // Auto-trust mise if the repo uses it
        if repo_path.join(".mise.toml").exists() || repo_path.join("mise.toml").exists() {
            let _ = Command::new("mise")
                .arg("trust")
                .current_dir(&worktree_dir)
                .output();
        }

        // Phase 3: Build launch command
        let cmd = if agent == AgentKind::ClaudeTui {
            // ClaudeTui needs -d (project root) and --worktree-id for inbox polling.
            // Write prompt to a temp file to avoid newlines breaking shell args.
            use crate::core::shell::shell_quote;
            let exe = std::env::current_exe()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "swarm".to_string());
            let prompt_file = format!("/tmp/swarm-prompt-{}.txt", window_name);
            std::fs::write(&prompt_file, prompt).unwrap_or_else(|e| {
                swarm_log!("[swarm] failed to write prompt file {prompt_file}: {e}");
            });
            format!(
                "'{}' -d {} agent-tui --dangerously-skip-permissions --worktree-id {} --prompt-file {}",
                exe,
                shell_quote(&self.work_dir.to_string_lossy()),
                shell_quote(&window_name),
                shell_quote(&prompt_file),
            )
        } else {
            agent.launch_cmd_with_prompt(prompt, true)
        };

        // Phase 4: Create tmux pane running the command directly (no shell)
        let has_live_agents = self.worktrees.iter().any(|w| {
            w.agent
                .as_ref()
                .is_some_and(|a| a.status == PaneStatus::Running)
        });

        let dir_str = worktree_dir.to_string_lossy();
        let pane_result = if !has_live_agents {
            let split_from = self
                .sidebar_pane_id
                .clone()
                .unwrap_or_else(|| "%0".to_string());
            tmux::split_pane_horizontal_with_cmd(&split_from, &dir_str, 70, &cmd)
        } else {
            let last_agent_pane = self
                .worktrees
                .iter()
                .rev()
                .filter_map(|w| {
                    w.agent
                        .as_ref()
                        .filter(|a| a.status == PaneStatus::Running)
                        .map(|a| a.pane_id.clone())
                })
                .next()
                .unwrap_or_else(|| {
                    self.sidebar_pane_id
                        .clone()
                        .unwrap_or_else(|| "%0".to_string())
                });
            tmux::split_pane_vertical_with_cmd(&last_agent_pane, &dir_str, &cmd)
        };

        let pane_id = match pane_result {
            Ok(id) => id,
            Err(e) => {
                swarm_log!(
                    "[swarm] tmux pane creation failed for {}: {}",
                    window_name, e
                );
                // Rollback: clean up git worktree
                let _ = git::remove_worktree(&repo_path, &worktree_dir);
                let _ = git::prune_worktrees(&repo_path);
                let _ = git::delete_branch(&repo_path, &branch_name);
                self.worktrees.remove(idx);
                self.save_state();
                return Err(e);
            }
        };

        // Set pane title to truncated prompt so it matches the sidebar
        let pane_title = if prompt.len() > 60 {
            format!(
                "{}…",
                &prompt[..prompt
                    .char_indices()
                    .take_while(|&(i, _)| i < 60)
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(60)]
            )
        } else {
            prompt.to_string()
        };
        let _ = tmux::set_pane_title(&pane_id, &pane_title);

        // Pane created with command running → transition to Running
        self.worktrees[idx].agent = Some(TrackedPane {
            pane_id: pane_id.clone(),
            status: PaneStatus::Running,
        });
        self.transition(idx, WorkerPhase::Starting, Some("pane created"));
        self.transition(idx, WorkerPhase::Running, Some("command launched"));

        self.selected = idx;

        // Rebalance layout and re-apply styling (after push so the new pane is included)
        self.rebalance_layout();
        let _ = tmux::apply_session_style(&self.session_name);

        // Re-select sidebar pane so TUI keeps focus
        if let Some(ref sidebar) = self.sidebar_pane_id {
            let _ = tmux::select_pane(sidebar);
        }

        // Apply per-pane colors/selection AFTER layout + session style
        // so they aren't overwritten by rebalance or apply_session_style
        self.prev_selected = None;
        self.update_pane_selection();

        // Request LLM-generated summary for the task prompt
        self.request_summary(window_name.clone(), prompt.to_string());

        // Emit event
        let _ = ipc::emit_event(
            &self.work_dir,
            &ipc::SwarmEvent::WorktreeCreated {
                worktree: window_name,
                branch: branch_name,
                agent: agent.label().to_string(),
                pane_id,
                timestamp: Local::now(),
            },
        );

        self.flash(format!("{} launched", agent.label()));
        Ok(())
    }

    // ── Merge Flow ─────────────────────────────────────────

    pub fn start_merge_selected(&mut self) {
        if self.worktrees.is_empty() {
            return;
        }
        let wt = &self.worktrees[self.selected];
        if wt.branch.is_empty() {
            self.flash("nothing to merge".to_string());
            return;
        }
        self.confirm_message = format!("merge {} into base? (y/n)", wt.branch);
        self.pending_action = Some(PendingAction::Merge(self.selected));
        self.mode = Mode::Confirm;
    }

    pub fn start_close_selected(&mut self) {
        if self.worktrees.is_empty() {
            return;
        }
        self.confirm_message = format!("close {}? (y/n)", self.worktrees[self.selected].id);
        self.pending_action = Some(PendingAction::Close(self.selected));
        self.mode = Mode::Confirm;
    }

    pub async fn confirm_action(&mut self) {
        let action = self.pending_action.take();
        self.mode = Mode::Normal;

        match action {
            Some(PendingAction::Merge(idx)) => {
                if let Err(e) = self.merge_worktree(idx) {
                    self.flash(format!("merge failed: {}", e));
                } else {
                    self.flash("merged".to_string());
                }
            }
            Some(PendingAction::Close(idx)) => {
                if let Err(e) = self.close_worktree(idx) {
                    self.flash(format!("close failed: {}", e));
                } else {
                    self.flash("closed".to_string());
                }
            }
            None => {}
        }
    }

    fn merge_worktree(&mut self, idx: usize) -> Result<()> {
        let wt = &self.worktrees[idx];
        let branch = wt.branch.clone();
        let commit_msg = format!("swarm: {}", wt.prompt);
        merge::commit_all(&wt.worktree_path, &commit_msg)?;
        let base = git::current_branch(&wt.repo_path)?;
        merge::merge_into_base(&wt.repo_path, &wt.branch, &base)?;

        // Emit merge event before closing
        let _ = ipc::emit_event(
            &self.work_dir,
            &ipc::SwarmEvent::WorktreeMerged {
                worktree: wt.id.clone(),
                branch: branch.clone(),
                timestamp: Local::now(),
            },
        );

        self.close_worktree(idx)?;
        Ok(())
    }

    fn close_worktree(&mut self, idx: usize) -> Result<()> {
        if idx >= self.worktrees.len() {
            return Ok(());
        }

        let wt = self.worktrees.remove(idx);

        // Kill agent pane
        if let Some(ref agent) = wt.agent {
            let _ = tmux::kill_pane(&agent.pane_id);
        }

        // Kill all terminal panes
        for term in &wt.terminals {
            let _ = tmux::kill_pane(&term.pane_id);
        }

        // Rebalance layout after removing panes
        self.rebalance_layout();

        // Remove git worktree and branch. Prune handles the case where the
        // directory is already gone but git still tracks the worktree entry.
        let _ = git::remove_worktree(&wt.repo_path, &wt.worktree_path);
        let _ = git::prune_worktrees(&wt.repo_path);
        let _ = git::delete_branch(&wt.repo_path, &wt.branch);

        if self.selected >= self.worktrees.len() && !self.worktrees.is_empty() {
            self.selected = self.worktrees.len() - 1;
        }

        // Emit event
        let _ = ipc::emit_event(
            &self.work_dir,
            &ipc::SwarmEvent::WorktreeClosed {
                worktree: wt.id.clone(),
                timestamp: Local::now(),
            },
        );

        self.save_state();
        self.prev_selected = None;
        self.update_pane_selection();
        Ok(())
    }

    // ── Help ───────────────────────────────────────────────

    pub fn toggle_help(&mut self) {
        if self.mode == Mode::Help {
            self.mode = Mode::Normal;
            self.show_help = false;
        } else {
            self.mode = Mode::Help;
            self.show_help = true;
        }
    }

    // ── Flash Messages ─────────────────────────────────────

    fn flash(&mut self, msg: String) {
        self.status_message = Some((msg, Instant::now()));
    }

    pub fn current_status(&self) -> Option<&str> {
        self.status_message.as_ref().and_then(|(msg, when)| {
            let duration = if msg.starts_with("error") {
                10
            } else if msg.starts_with("http") {
                30
            } else {
                4
            };
            if when.elapsed().as_secs() < duration {
                Some(msg.as_str())
            } else {
                None
            }
        })
    }

    // ── IPC: Process Inbox ─────────────────────────────────

    /// Handle a single inbox message (dispatches Create/Send/Close/Merge).
    fn handle_inbox_message(&mut self, msg: ipc::InboxMessage) {
        match msg {
            ipc::InboxMessage::Create {
                prompt,
                agent,
                repo,
                start_point,
                ..
            } => {
                let agent_kind = AgentKind::from_str(&agent).unwrap_or(AgentKind::ClaudeTui);
                let repo_path = match &repo {
                    Some(name) => {
                        match self
                            .repos
                            .iter()
                            .find(|r| git::repo_name(r) == *name)
                            .cloned()
                        {
                            Some(path) => path,
                            None => {
                                let names: Vec<_> =
                                    self.repos.iter().map(|r| git::repo_name(r)).collect();
                                let err = format!(
                                    "unknown repo '{}' (available: {})",
                                    name,
                                    names.join(", ")
                                );
                                self.flash(format!("create failed: {}", err));
                                let _ = ipc::emit_event(
                                    &self.work_dir,
                                    &ipc::SwarmEvent::CreateFailed {
                                        error: err,
                                        prompt,
                                        repo,
                                        timestamp: Local::now(),
                                    },
                                );
                                return;
                            }
                        }
                    }
                    None if self.repos.len() > 1 => {
                        let names: Vec<_> = self.repos.iter().map(|r| git::repo_name(r)).collect();
                        let err = format!("--repo required ({})", names.join(", "));
                        self.flash(format!("create failed: {}", err));
                        let _ = ipc::emit_event(
                            &self.work_dir,
                            &ipc::SwarmEvent::CreateFailed {
                                error: err,
                                prompt,
                                repo,
                                timestamp: Local::now(),
                            },
                        );
                        return;
                    }
                    None => self
                        .repos
                        .first()
                        .cloned()
                        .unwrap_or_else(|| self.work_dir.clone()),
                };
                if let Err(e) = self.create_worktree_with_agent(
                    &prompt,
                    agent_kind,
                    &repo_path,
                    start_point.as_deref(),
                ) {
                    let err = format!("{}", e);
                    self.flash(format!("inbox create error: {}", err));
                    let _ = ipc::emit_event(
                        &self.work_dir,
                        &ipc::SwarmEvent::CreateFailed {
                            error: err,
                            prompt,
                            repo,
                            timestamp: Local::now(),
                        },
                    );
                }
            }
            ipc::InboxMessage::Send {
                worktree, message, ..
            } => {
                if let Some(wt) = self.worktrees.iter().find(|w| w.id == worktree) {
                    if wt.agent_kind == AgentKind::ClaudeTui {
                        // Write to per-agent inbox — the agent-tui polls this directly
                        let _ = ipc::write_agent_inbox(&self.work_dir, &worktree, &message);
                    } else if let Some(ref agent) = wt.agent {
                        let _ = tmux::send_keys_to_pane(&agent.pane_id, &message);
                    }
                }
            }
            ipc::InboxMessage::Close { worktree, .. } => {
                if let Some(idx) = self.worktrees.iter().position(|w| w.id == worktree) {
                    let _ = self.close_worktree(idx);
                }
            }
            ipc::InboxMessage::Merge { worktree, .. } => {
                if let Some(idx) = self.worktrees.iter().position(|w| w.id == worktree) {
                    let _ = self.merge_worktree(idx);
                }
            }
        }
    }

    /// Drain all pending messages from the Unix domain socket channel.
    fn drain_socket_inbox(&mut self) {
        while let Ok(msg) = self.inbox_rx.try_recv() {
            self.handle_inbox_message(msg);
        }
    }

    /// Drain all pending messages from the JSONL file inbox.
    fn drain_file_inbox(&mut self) {
        let (messages, new_pos) = match ipc::read_inbox(&self.work_dir, self.last_inbox_pos) {
            Ok(result) => result,
            Err(_) => return,
        };

        if new_pos == self.last_inbox_pos {
            return;
        }
        self.last_inbox_pos = new_pos;

        // Compact the inbox once processed data exceeds 64 KB.
        let inbox_path = self.work_dir.join(".swarm").join("inbox.jsonl");
        if self.last_inbox_pos > 64 * 1024
            && let Ok(contents) = std::fs::read(&inbox_path)
        {
            let pos = self.last_inbox_pos as usize;
            let remaining = if pos < contents.len() {
                &contents[pos..]
            } else {
                &[]
            };
            let _ = std::fs::write(&inbox_path, remaining);
            self.last_inbox_pos = 0;
        }

        if messages.is_empty() {
            // Position advanced (e.g. blank lines) — persist it
            self.save_state();
            return;
        }

        for msg in messages {
            self.handle_inbox_message(msg);
        }
    }

    // ── PR Status ──────────────────────────────────────────

    fn refresh_pr_statuses(&mut self) {
        let all_repos = self.repos.clone();
        let mut merged_ids = Vec::new();

        // Snapshot PR state before lookups to detect new PRs
        let prev_prs: Vec<Option<PrInfo>> =
            self.worktrees.iter().map(|wt| wt.pr.clone()).collect();

        // Collect all branches so each lookup can skip other worktrees' branches
        let all_branches: Vec<String> =
            self.worktrees.iter().map(|w| w.branch.clone()).collect();

        for (i, wt) in &mut self.worktrees.iter_mut().enumerate() {
            let other_branches: Vec<String> = all_branches
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, b)| b.clone())
                .collect();
            if lookup_pr_for_worktree(wt, &all_repos, &other_branches) {
                merged_ids.push(wt.id.clone());
            }
        }

        // Emit PrDetected events for worktrees where PR went from None to Some
        // (or URL changed)
        for (idx, prev) in prev_prs.iter().enumerate() {
            if idx >= self.worktrees.len() {
                break;
            }
            let wt = &self.worktrees[idx];
            if let Some(ref pr) = wt.pr {
                let is_new = match prev {
                    None => true,
                    Some(old) => old.url != pr.url,
                };
                if is_new {
                    let _ = ipc::emit_event(
                        &self.work_dir,
                        &ipc::SwarmEvent::PrDetected {
                            worktree: wt.id.clone(),
                            pr_url: pr.url.clone(),
                            pr_title: pr.title.clone(),
                            pr_number: pr.number,
                            timestamp: Local::now(),
                        },
                    );
                }
            }
        }

        // Persist updated PR info to state.json
        self.save_state();

        // Auto-close worktrees whose PRs were just merged
        for id in merged_ids {
            if let Some(idx) = self.worktrees.iter().position(|w| w.id == id) {
                swarm_log!("[swarm] Auto-closing worktree {} — PR merged", id);
                let prompt = self.worktrees[idx].prompt.clone();
                let _ = self.close_worktree(idx);
                self.flash(format!("auto-closed \"{}\" (PR merged)", prompt));
            }
        }
    }

    // ── Agent Status Polling ─────────────────────────────────

    /// Poll `.swarm/agent-status/<id>` files for each worktree. When a
    /// worktree's status newly transitions to "waiting", trigger an
    /// immediate PR lookup so the sidebar and state.json have fresh PR
    /// info without waiting for the 30s poll cycle.
    fn poll_agent_statuses(&mut self) {
        let status_dir = self.work_dir.join(".swarm").join("agent-status");
        let all_repos = self.repos.clone();
        let mut needs_save = false;
        let mut newly_waiting: Vec<usize> = Vec::new();
        let mut phase_transitions: Vec<(usize, WorkerPhase)> = Vec::new();

        for (idx, wt) in self.worktrees.iter_mut().enumerate() {
            let new_status = std::fs::read_to_string(status_dir.join(&wt.id))
                .ok()
                .and_then(|s| {
                    let trimmed = s.trim().to_string();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed)
                    }
                });

            let was_waiting = wt.agent_session_status.as_deref() == Some("waiting");
            let is_waiting = new_status.as_deref() == Some("waiting");
            let is_running = new_status.as_deref() == Some("running");

            if wt.agent_session_status != new_status {
                wt.agent_session_status = new_status;
                needs_save = true;
            }

            // Phase transitions based on agent-status file
            if is_waiting && wt.phase == WorkerPhase::Running {
                phase_transitions.push((idx, WorkerPhase::Waiting));
            } else if is_running && wt.phase == WorkerPhase::Waiting {
                phase_transitions.push((idx, WorkerPhase::Running));
            }

            // Newly transitioned to waiting — queue immediate PR lookup,
            // but skip very new workers (< 60s old) that can't possibly have a PR yet.
            if is_waiting && !was_waiting {
                let age = Local::now().signed_duration_since(wt.created_at);
                if age.num_seconds() >= 60 {
                    newly_waiting.push(idx);
                }
            }
        }

        // Apply phase transitions
        for (idx, to) in phase_transitions {
            self.transition(idx, to, Some("agent-status file"));
        }

        // Run PR lookups for workers that just became waiting.
        // Always re-check — even if a PR was already found by the 30s poll —
        // because the PR may have been merged since the last check.
        if !newly_waiting.is_empty() {
            let all_branches: Vec<String> =
                self.worktrees.iter().map(|w| w.branch.clone()).collect();
            let mut any_merged = Vec::new();
            for idx in &newly_waiting {
                if *idx >= self.worktrees.len() {
                    continue;
                }
                let other_branches: Vec<String> = all_branches
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != *idx)
                    .map(|(_, b)| b.clone())
                    .collect();
                let wt = &mut self.worktrees[*idx];
                swarm_log!("[swarm] Agent waiting — immediate PR lookup for {}", wt.id);
                if lookup_pr_for_worktree(wt, &all_repos, &other_branches) {
                    any_merged.push(wt.id.clone());
                }
            }
            needs_save = true;

            // Auto-close any that were merged
            for id in any_merged {
                if let Some(idx) = self.worktrees.iter().position(|w| w.id == id) {
                    swarm_log!("[swarm] Auto-closing worktree {} — PR merged", id);
                    let prompt = self.worktrees[idx].prompt.clone();
                    let _ = self.close_worktree(idx);
                    self.flash(format!("auto-closed \"{}\" (PR merged)", prompt));
                }
            }
        }

        if needs_save {
            self.save_state();
        }
    }

    // ── Summary Generation ─────────────────────────────────

    fn request_summary(&self, worktree_id: String, prompt: String) {
        let tx = self.summary_tx.clone();
        tokio::spawn(async move {
            if let Some(summary) = generate_summary_via_claude(&prompt).await {
                let _ = tx.send((worktree_id, summary));
            }
        });
    }

    fn collect_summaries(&mut self) {
        let mut changed = false;
        while let Ok((id, summary)) = self.summary_rx.try_recv() {
            if let Some(wt) = self.worktrees.iter_mut().find(|w| w.id == id) {
                wt.summary = Some(summary);
                changed = true;
            }
        }
        if changed {
            self.save_state();
        }
    }

    // ── Tick ───────────────────────────────────────────────

    pub fn tick(&mut self) {
        self.tick_count += 1;

        self.collect_summaries(); // non-blocking channel drain
        self.drain_socket_inbox(); // non-blocking channel drain

        // ── TEMPORARILY DISABLED — restore one at a time after j/k verified smooth ──

        // Apply deferred pane border styling (batched into single tmux source-file)
        if self.pane_style_dirty {
            self.pane_style_dirty = false;
            self.update_pane_selection();
        }

        // // Retry layout rebalance (blocking tmux calls)
        // if self.layout_dirty {
        //     self.try_rebalance_layout();
        //     if !self.layout_dirty {
        //         let _ = tmux::apply_session_style(&self.session_name);
        //     }
        // }

        // // Poll file inbox every 2s (file I/O)
        // if self.last_inbox_check.elapsed().as_secs() >= 2 {
        //     self.drain_file_inbox();
        //     self.last_inbox_check = Instant::now();
        // }

        // // Refresh pane states (tmux list-panes) + poll agent statuses (file + gh CLI) every 3s
        // if self.last_refresh.elapsed().as_secs() >= 3 {
        //     self.refresh_pane_states();
        //     self.poll_agent_statuses();
        //     self.last_refresh = Instant::now();
        // }

        // // Refresh PR statuses (gh CLI × N worktrees) every 30s
        // if self.last_pr_check.elapsed().as_secs() >= 30 {
        //     self.refresh_pr_statuses();
        //     self.last_pr_check = Instant::now();
        // }
    }

    fn refresh_pane_states(&mut self) {
        // Check which panes are still alive
        let session_window = self.session_name.clone();
        let live_pane_ids: Vec<String> = tmux::list_panes(&session_window)
            .unwrap_or_default()
            .iter()
            .map(|p| p.pane_id.clone())
            .collect();

        // Track which agents JUST transitioned to done (fix double-emit bug:
        // previously emitted AgentDone for ALL done agents when any_done was true)
        let mut just_done: Vec<usize> = Vec::new();

        for (idx, wt) in self.worktrees.iter_mut().enumerate() {
            if let Some(ref mut agent) = wt.agent
                && agent.status == PaneStatus::Running
                && !live_pane_ids.contains(&agent.pane_id)
            {
                agent.status = PaneStatus::Done;
                just_done.push(idx);
            }
            for term in &mut wt.terminals {
                if term.status == PaneStatus::Running && !live_pane_ids.contains(&term.pane_id) {
                    term.status = PaneStatus::Done;
                }
            }
        }

        // Emit AgentDone events and transition to Completed only for agents that JUST died
        for idx in just_done {
            let wt = &self.worktrees[idx];
            let _ = ipc::emit_event(
                &self.work_dir,
                &ipc::SwarmEvent::AgentDone {
                    worktree: wt.id.clone(),
                    timestamp: Local::now(),
                },
            );

            // Transition active phases to Completed
            if self.worktrees[idx].phase.is_active() {
                self.transition(idx, WorkerPhase::Completed, Some("pane exited"));
            }
        }
    }

    // ── Helpers ────────────────────────────────────────────

    fn ensure_session(&self) -> Result<()> {
        if !tmux::session_exists(&self.session_name) {
            tmux::create_session(&self.session_name, &self.work_dir.to_string_lossy())?;
            tmux::apply_session_style(&self.session_name)?;
        }
        Ok(())
    }

    fn worktree_base(&self) -> PathBuf {
        self.work_dir.join(".swarm").join("wt")
    }

    /// Get the border color hex for a worktree by its index.
    fn worktree_border_color(&self, idx: usize) -> &'static str {
        WORKTREE_BORDER_COLORS[idx % WORKTREE_BORDER_COLORS.len()]
    }

    /// Build the per-pane border format string for a worktree.
    fn pane_border_fmt(color: &str, selected: bool) -> String {
        if selected {
            // Selected: bright color + bold, full-width fill
            format!(
                "#[fg={},bold]\u{2501}\u{2501} #{{pane_title}} {}#[default]",
                color,
                "\u{2501}".repeat(128)
            )
        } else {
            // Non-selected: dim gray, short
            "#[fg=#5a5550]\u{2501}\u{2501} #{pane_title} #[default]".to_string()
        }
    }

    /// Update pane selection styling — dims non-selected worktrees, brightens selected.
    /// Sets per-pane background/foreground AND per-pane border format with hardcoded colors.
    /// Uses delta updates when possible (only touches changed worktrees).
    /// Called from tick() (not from j/k directly) so navigation is never blocked.
    fn update_pane_selection(&mut self) {
        if self.worktrees.is_empty() {
            self.prev_selected = None;
            return;
        }

        let selected = self.selected;

        // Determine which worktree indices to update
        let indices_to_update: Vec<usize> = if let Some(prev) = self.prev_selected {
            if prev == selected {
                return; // No change
            }
            // Delta: only update the two changed worktrees
            let mut v = vec![selected];
            if prev < self.worktrees.len() {
                v.push(prev);
            }
            v
        } else {
            // Full update (first call or after add/remove)
            (0..self.worktrees.len()).collect()
        };

        // Build all tmux set-option commands into a single batch to avoid
        // spawning 6+ subprocesses (the old approach caused j/k navigation lag).
        let mut cmds = String::new();

        for idx in indices_to_update {
            let is_selected = idx == selected;
            let fg = if is_selected {
                PANE_FG_SELECTED
            } else {
                PANE_FG_DIMMED
            };
            let bg = if is_selected {
                PANE_BG_SELECTED
            } else {
                PANE_BG_DIMMED
            };
            let style = format!("bg={},fg={}", bg, fg);
            let color = self.worktree_border_color(idx);
            let border_fmt = Self::pane_border_fmt(color, is_selected);
            let border_style = if is_selected {
                format!("fg={},bg=#302c26", color)
            } else {
                "fg=#4a4540,bg=#302c26".to_string()
            };

            if let Some(wt) = self.worktrees.get(idx) {
                // Collect pane IDs that need updating
                let mut pane_ids: Vec<&str> = Vec::new();
                if let Some(ref agent) = wt.agent
                    && agent.status == PaneStatus::Running
                {
                    pane_ids.push(&agent.pane_id);
                }
                for term in &wt.terminals {
                    if term.status == PaneStatus::Running {
                        pane_ids.push(&term.pane_id);
                    }
                }
                for pane_id in pane_ids {
                    use std::fmt::Write;
                    let _ = writeln!(cmds, "set-option -p -t {pane_id} style \"{style}\"");
                    let _ = writeln!(
                        cmds,
                        "set-option -p -t {pane_id} pane-border-format \"{border_fmt}\""
                    );
                    let _ = writeln!(
                        cmds,
                        "set-option -p -t {pane_id} pane-border-style \"{border_style}\""
                    );
                }
            }
        }

        if !cmds.is_empty() {
            let _ = tmux::source_commands(&cmds);
        }

        self.prev_selected = Some(selected);
    }

    /// Rebalance tmux pane layout into a tiled grid.
    /// Sets `layout_dirty` so the tick handler retries if this attempt fails.
    pub fn rebalance_layout(&mut self) {
        self.layout_dirty = true;
        self.try_rebalance_layout();
    }

    /// Attempt to apply the tiled layout. Clears `layout_dirty` on success.
    fn try_rebalance_layout(&mut self) {
        let session_window = self.session_name.clone();
        let sidebar = self
            .sidebar_pane_id
            .clone()
            .unwrap_or_else(|| "%0".to_string());

        // Collect live pane IDs grouped by worktree
        let pane_groups: Vec<Vec<String>> = self
            .worktrees
            .iter()
            .map(|wt| {
                let mut panes = Vec::new();
                if let Some(ref agent) = wt.agent
                    && agent.status == PaneStatus::Running
                {
                    panes.push(agent.pane_id.clone());
                }
                for term in &wt.terminals {
                    if term.status == PaneStatus::Running {
                        panes.push(term.pane_id.clone());
                    }
                }
                panes
            })
            .collect();

        if tmux::apply_tiled_layout(&session_window, &sidebar, 38, pane_groups).is_ok() {
            self.layout_dirty = false;
        }
    }

    fn next_worktree_num(&self) -> usize {
        let max = self
            .worktrees
            .iter()
            .filter_map(|w| {
                w.id.rsplit('-')
                    .next()
                    .and_then(|n| n.parse::<usize>().ok())
            })
            .max()
            .unwrap_or(0);
        max + 1
    }

    pub fn repo_display_name(&self) -> String {
        if self.repos.len() == 1 {
            git::repo_name(&self.repos[0])
        } else if self.repos.is_empty() {
            self.work_dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "swarm".to_string())
        } else {
            format!("{} repos", self.repos.len())
        }
    }
}

/// Look up PR status for a single worktree via `gh pr list`.
/// Tries the assigned branch first, then falls back to the worktree's actual
/// current branch (workers often create their own). Returns `true` if the PR
/// just transitioned to MERGED (caller should auto-close).
fn lookup_pr_for_worktree(wt: &mut Worktree, all_repos: &[PathBuf], other_branches: &[String]) -> bool {
    if wt.branch.is_empty() {
        return false;
    }

    // Build list of repos to search: wt.repo_path first, then others.
    // Clone into owned vec to avoid borrowing wt while passing &mut wt.
    let mut repos_to_try: Vec<PathBuf> = vec![wt.repo_path.clone()];
    for repo in all_repos {
        if *repo != wt.repo_path {
            repos_to_try.push(repo.clone());
        }
    }
    let repo_refs: Vec<&PathBuf> = repos_to_try.iter().collect();

    let branch = wt.branch.clone();

    // Try the assigned branch first
    if let Some(newly_merged) = try_pr_lookup(wt, &branch, &repo_refs, None) {
        return newly_merged;
    }

    // Fallback: check the worktree's actual current branch. Workers
    // (especially claude-tui) often create their own branch instead of
    // using the swarm-assigned one.
    let actual_branch = Command::new("git")
        .arg("-C")
        .arg(&wt.worktree_path)
        .args(["branch", "--show-current"])
        .output()
        .ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !s.is_empty() && s != wt.branch {
                Some(s)
            } else {
                None
            }
        });

    if let Some(ref actual) = actual_branch {
        swarm_log!(
            "[swarm] Branch fallback for {}: assigned '{}', actual '{}'",
            wt.id, wt.branch, actual
        );
        if let Some(newly_merged) = try_pr_lookup(wt, actual, &repo_refs, Some(actual)) {
            return newly_merged;
        }
    }

    // Strategy 3: Query recent open PRs and match by checking which head branch
    // exists locally in the worktree. Workers sometimes create their own branch
    // for the PR without switching the worktree HEAD.
    let mut skip = vec![branch];
    if let Some(ref actual) = actual_branch {
        skip.push(actual.clone());
    }
    skip.extend_from_slice(other_branches);
    if let Some(newly_merged) = try_pr_lookup_by_worktree_branches(wt, &repo_refs, &skip) {
        return newly_merged;
    }

    // Strategy 4: If we already know the PR from a previous lookup, check its
    // current state directly by number. This handles the case where a PR was
    // found via strategy 3 (local branch matching) and then merged — strategy 3
    // only matches OPEN PRs, so it misses the merged PR. A direct `gh pr view`
    // catches the state transition and allows auto-close to fire.
    if let Some(ref known_pr) = wt.pr
        && let Some(newly_merged) = try_pr_lookup_by_number(wt, known_pr.number, &repo_refs)
    {
        return newly_merged;
    }

    wt.pr = None;
    false
}

/// Try `gh pr list --head <branch>` across the given repos. On success,
/// updates `wt.pr` and returns `Some(true)` if newly merged, `Some(false)`
/// if found but not newly merged. Returns `None` if no PR was found.
fn try_pr_lookup(
    wt: &mut Worktree,
    branch: &str,
    repos_to_try: &[&PathBuf],
    fallback_label: Option<&str>,
) -> Option<bool> {
    for repo_dir in repos_to_try {
        let output = Command::new("gh")
            .args([
                "pr",
                "list",
                "--head",
                branch,
                "--state",
                "all",
                "--json",
                "number,title,state,url",
                "--limit",
                "1",
            ])
            .current_dir(repo_dir)
            .output();

        if let Ok(output) = output
            && output.status.success()
        {
            let text = String::from_utf8_lossy(&output.stdout);
            if let Ok(prs) = serde_json::from_str::<Vec<serde_json::Value>>(text.trim())
                && let Some(pr) = prs.first()
            {
                let new_pr = PrInfo {
                    number: pr["number"].as_u64().unwrap_or(0),
                    title: pr["title"].as_str().unwrap_or("").to_string(),
                    state: pr["state"].as_str().unwrap_or("").to_string(),
                    url: pr["url"].as_str().unwrap_or("").to_string(),
                };
                let newly_merged = new_pr.is_newly_merged(wt.pr.as_ref());
                let is_new = wt.pr.is_none();
                let state_changed = wt.pr.as_ref().is_some_and(|old| old.state != new_pr.state);
                if is_new {
                    if let Some(label) = fallback_label {
                        swarm_log!(
                            "[swarm] PR detected (via actual branch '{label}'): #{} \"{}\" ({}) {}",
                            new_pr.number, new_pr.title, new_pr.state, new_pr.url
                        );
                    } else {
                        swarm_log!(
                            "[swarm] PR detected: #{} \"{}\" ({}) {}",
                            new_pr.number, new_pr.title, new_pr.state, new_pr.url
                        );
                    }
                } else if state_changed {
                    swarm_log!(
                        "[swarm] PR updated: #{} state -> {} {}",
                        new_pr.number, new_pr.state, new_pr.url
                    );
                }
                wt.pr = Some(new_pr);
                return Some(newly_merged);
            }
        }
    }
    None
}

/// Returns the set of branch names currently checked out in OTHER worktrees
/// (i.e., not in `this_worktree_path`). Used to avoid cross-contaminating
/// PR detection when `git branch --list` returns branches from all worktrees.
fn branches_in_other_worktrees(this_worktree_path: &Path) -> HashSet<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(this_worktree_path)
        .args(["worktree", "list", "--porcelain"])
        .output();

    let mut result = HashSet::new();
    let Ok(output) = output else { return result };
    if !output.status.success() {
        return result;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;

    for line in text.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            // New worktree block — save previous
            if let (Some(path), Some(branch)) = (current_path.take(), current_branch.take())
                && path != this_worktree_path
            {
                result.insert(branch);
            }
            current_path = Some(PathBuf::from(path));
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
            current_branch = Some(branch.to_string());
        }
    }
    // Handle last block
    if let (Some(path), Some(branch)) = (current_path, current_branch)
        && path != this_worktree_path
    {
        result.insert(branch);
    }
    result
}

/// Strategy 3: get ALL local branches in the worktree, query recent PRs
/// (`--state all`) for the repo in ONE call, and match any PR whose
/// `headRefName` is in the local branch list. O(1) API calls, covers the
/// case where a worker creates its own branch without switching HEAD.
fn try_pr_lookup_by_worktree_branches(
    wt: &mut Worktree,
    repos_to_try: &[&PathBuf],
    skip_branches: &[String],
) -> Option<bool> {
    // 1. Get all local branches in the worktree
    let branch_output = Command::new("git")
        .arg("-C")
        .arg(&wt.worktree_path)
        .args(["branch", "--list", "--format=%(refname:short)"])
        .output()
        .ok()?;

    if !branch_output.status.success() {
        return None;
    }

    let local_branches: Vec<String> = String::from_utf8_lossy(&branch_output.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && !skip_branches.iter().any(|skip| skip == s))
        .collect();

    // Filter out branches checked out in other worktrees to avoid
    // cross-contaminating PR detection.
    let other_wt_branches = branches_in_other_worktrees(&wt.worktree_path);
    let local_branches: Vec<String> = local_branches
        .into_iter()
        .filter(|b| !other_wt_branches.contains(b))
        .collect();

    if local_branches.is_empty() {
        return None;
    }

    let branch_refs: Vec<&str> = local_branches.iter().map(|s| s.as_str()).collect();

    // 2a. Get local branch tip commit SHAs (one git call, no network)
    let branch_tips: HashMap<String, String> = Command::new("git")
        .arg("-C")
        .arg(&wt.worktree_path)
        .args(["for-each-ref", "--format=%(refname:short) %(objectname:short)", "refs/heads/"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|line| {
                    let mut parts = line.splitn(2, ' ');
                    let branch = parts.next()?.to_string();
                    let sha = parts.next()?.to_string();
                    Some((branch, sha))
                })
                .collect()
        })
        .unwrap_or_default();

    // 2b. Query all recent PRs for each repo (one API call per repo)
    for repo_dir in repos_to_try {
        let output = Command::new("gh")
            .args([
                "pr",
                "list",
                "--state",
                "all",
                "--json",
                "number,title,state,url,headRefName,headRefOid,updatedAt",
                "--limit",
                "20",
            ])
            .current_dir(repo_dir)
            .output();

        if let Ok(output) = output
            && output.status.success()
        {
            let text = String::from_utf8_lossy(&output.stdout);
            if let Ok(prs) = serde_json::from_str::<Vec<serde_json::Value>>(text.trim())
                // 3. Match using the pure function with commit-aware heuristics
                && let Some(pr) = match_pr_by_local_branches(&branch_refs, &prs, &branch_tips)
            {
                let head_ref = pr["headRefName"].as_str().unwrap_or("").to_string();
                let new_pr = PrInfo {
                    number: pr["number"].as_u64().unwrap_or(0),
                    title: pr["title"].as_str().unwrap_or("").to_string(),
                    state: pr["state"].as_str().unwrap_or("").to_string(),
                    url: pr["url"].as_str().unwrap_or("").to_string(),
                };
                let newly_merged = new_pr.is_newly_merged(wt.pr.as_ref());
                let is_new = wt.pr.is_none();
                let state_changed = wt.pr.as_ref().is_some_and(|old| old.state != new_pr.state);
                if is_new {
                    swarm_log!(
                        "[swarm] PR detected (via worktree branch '{head_ref}'): #{} \"{}\" ({}) {}",
                        new_pr.number, new_pr.title, new_pr.state, new_pr.url
                    );
                } else if state_changed {
                    swarm_log!(
                        "[swarm] PR updated: #{} state -> {} {}",
                        new_pr.number, new_pr.state, new_pr.url
                    );
                }
                wt.pr = Some(new_pr);
                return Some(newly_merged);
            }
        }
    }
    None
}

/// Pure matching: find the best OPEN PR whose `headRefName` is in
/// `local_branches`.  Uses layered heuristics to pick the most relevant PR
/// when a worker opens multiple PRs from the same worktree:
///
///   1. **Commit match** — PR whose `headRefOid` prefix-matches the local
///      branch tip (actively pushed, not stale).
///   2. **Most recently updated** — `updatedAt` timestamp from GitHub.
///   3. **Highest PR number** — monotonically increasing, so newest PR wins.
///
/// `branch_tips` maps branch name → short commit SHA (from `git for-each-ref`).
/// Only matches OPEN PRs — merged/closed PRs on stale local branches are
/// false positives that would trigger spurious auto-close.
/// No subprocess calls — suitable for unit testing.
fn match_pr_by_local_branches<'a>(
    local_branches: &[&str],
    prs: &'a [serde_json::Value],
    branch_tips: &HashMap<String, String>,
) -> Option<&'a serde_json::Value> {
    let mut best: Option<&serde_json::Value> = None;
    let mut best_score: (bool, &str, u64) = (false, "", 0);
    // Score tuple: (commit_matches, updatedAt, number) — compared lexicographically.

    for pr in prs {
        let head_ref = pr["headRefName"].as_str().unwrap_or("");
        let state = pr["state"].as_str().unwrap_or("");
        let number = pr["number"].as_u64().unwrap_or(0);
        if head_ref.is_empty() || state != "OPEN" || !local_branches.contains(&head_ref) {
            continue;
        }

        // Heuristic 1: does the PR's head commit match the local branch tip?
        let head_oid = pr["headRefOid"].as_str().unwrap_or("");
        let commit_matches = !head_oid.is_empty()
            && branch_tips
                .get(head_ref)
                .is_some_and(|local_sha| head_oid.starts_with(local_sha.as_str()) || local_sha.starts_with(head_oid));

        // Heuristic 2: most recently updated on GitHub
        let updated_at = pr["updatedAt"].as_str().unwrap_or("");

        let score = (commit_matches, updated_at, number);
        if score > best_score {
            best = Some(pr);
            best_score = score;
        }
    }
    best
}

/// Direct PR lookup by number via `gh pr view`. Used as a fallback when
/// branch-based strategies fail to find a previously-known PR (e.g. after
/// the PR is merged and strategy 3's OPEN-only filter skips it).
fn try_pr_lookup_by_number(
    wt: &mut Worktree,
    pr_number: u64,
    repos_to_try: &[&PathBuf],
) -> Option<bool> {
    for repo_dir in repos_to_try {
        let output = Command::new("gh")
            .args([
                "pr",
                "view",
                &pr_number.to_string(),
                "--json",
                "number,title,state,url",
            ])
            .current_dir(repo_dir)
            .output();

        if let Ok(output) = output
            && output.status.success()
        {
            let text = String::from_utf8_lossy(&output.stdout);
            // gh pr view returns a single object, not an array
            if let Ok(pr) = serde_json::from_str::<serde_json::Value>(text.trim()) {
                let new_pr = PrInfo {
                    number: pr["number"].as_u64().unwrap_or(0),
                    title: pr["title"].as_str().unwrap_or("").to_string(),
                    state: pr["state"].as_str().unwrap_or("").to_string(),
                    url: pr["url"].as_str().unwrap_or("").to_string(),
                };
                let newly_merged = new_pr.is_newly_merged(wt.pr.as_ref());
                let state_changed = wt.pr.as_ref().is_some_and(|old| old.state != new_pr.state);
                if state_changed {
                    swarm_log!(
                        "[swarm] PR #{} state -> {} (direct lookup)",
                        new_pr.number, new_pr.state
                    );
                }
                wt.pr = Some(new_pr);
                return Some(newly_merged);
            }
        }
    }
    None
}

/// Generate a short task summary using the Claude CLI.
/// Returns None if the CLI is unavailable or produces bad output.
async fn generate_summary_via_claude(prompt: &str) -> Option<String> {
    let output = tokio::process::Command::new("claude")
        .args([
            "--print",
            &format!(
                "Summarize this coding task in 4-6 words, lowercase, no punctuation: {}",
                prompt
            ),
        ])
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() || text.len() > 80 {
        return None;
    }
    Some(text)
}

/// Generate a branch name from a prompt and unique number.
///
/// Applies sanitization (lowercase, special chars -> hyphens, truncate to 40)
/// and prepends the "swarm/" prefix.
pub fn generate_branch_name(prompt: &str, num: usize) -> String {
    format!("swarm/{}-{}", sanitize(prompt), num)
}

/// Generate a worktree directory name from a prompt and unique number.
pub fn generate_worktree_dir_name(prompt: &str, num: usize) -> String {
    format!("{}-{}", sanitize(prompt), num)
}

/// Read agent session status from an agent-status file.
///
/// Returns the trimmed file contents, or None if the file is missing or empty.
pub fn read_agent_status(status_dir: &std::path::Path, id: &str) -> Option<String> {
    std::fs::read_to_string(status_dir.join(id))
        .ok()
        .and_then(|s| {
            let trimmed = s.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        })
}

/// Match a repo name against known repos, returning the path if found.
///
/// This is the pure matching logic extracted from process_inbox's Create handler.
/// Returns the matching repo path, or None with a list of available repo names.
pub fn find_repo_by_name<'a>(repos: &'a [PathBuf], name: &str) -> Option<&'a PathBuf> {
    repos.iter().find(|r| {
        r.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
            == name
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_pr(state: &str) -> PrInfo {
        PrInfo {
            number: 1,
            title: "test".to_string(),
            state: state.to_string(),
            url: "https://example.com/pr/1".to_string(),
        }
    }

    #[test]
    fn is_newly_merged_no_previous_pr() {
        let merged = make_pr("MERGED");
        assert!(merged.is_newly_merged(None));
    }

    #[test]
    fn is_newly_merged_from_open() {
        let merged = make_pr("MERGED");
        let prev = make_pr("OPEN");
        assert!(merged.is_newly_merged(Some(&prev)));
    }

    #[test]
    fn is_newly_merged_already_merged() {
        let merged = make_pr("MERGED");
        let prev = make_pr("MERGED");
        assert!(!merged.is_newly_merged(Some(&prev)));
    }

    #[test]
    fn is_newly_merged_open_pr() {
        let open = make_pr("OPEN");
        assert!(!open.is_newly_merged(None));
    }

    #[test]
    fn is_newly_merged_closed_not_merged() {
        let closed = make_pr("CLOSED");
        assert!(!closed.is_newly_merged(None));
    }

    #[test]
    fn worktree_to_state_preserves_pr() {
        let wt = Worktree {
            id: "test-1".to_string(),
            branch: "swarm/test-1".to_string(),
            prompt: "fix bug".to_string(),
            agent_kind: AgentKind::Claude,
            repo_path: PathBuf::from("/tmp/repo"),
            worktree_path: PathBuf::from("/tmp/wt"),
            created_at: Local::now(),
            agent: None,
            terminals: vec![],
            pr: Some(make_pr("MERGED")),
            phase: WorkerPhase::Running,
            summary: None,

            agent_session_status: None,
        };
        let state = wt.to_state();
        let pr = state.pr.expect("pr should persist");
        assert_eq!(pr.state, "MERGED");
        assert_eq!(pr.number, 1);
    }

    #[test]
    fn worktree_from_state_preserves_pr() {
        let ws = state::WorktreeState {
            id: "test-1".to_string(),
            branch: "swarm/test-1".to_string(),
            prompt: "fix bug".to_string(),
            agent_kind: AgentKind::Claude,
            repo_path: PathBuf::from("/tmp/repo"),
            worktree_path: PathBuf::from("/tmp/wt"),
            created_at: Local::now(),
            agent: None,
            terminals: vec![],
            summary: None,
            pr: Some(make_pr("OPEN")),
            phase: WorkerPhase::Completed,
            status: "done".to_string(),
            agent_session_status: None,
        };
        let wt = Worktree::from_state(&ws);
        let pr = wt.pr.expect("pr should restore");
        assert_eq!(pr.state, "OPEN");
    }

    #[test]
    fn to_state_propagates_agent_session_status() {
        let wt = Worktree {
            id: "test-1".to_string(),
            branch: "swarm/test-1".to_string(),
            prompt: "fix bug".to_string(),
            agent_kind: AgentKind::Claude,
            repo_path: PathBuf::from("/tmp/repo"),
            worktree_path: PathBuf::from("/tmp/wt"),
            created_at: Local::now(),
            agent: None,
            terminals: vec![],
            pr: None,
            phase: WorkerPhase::Waiting,
            summary: None,

            agent_session_status: Some("waiting".to_string()),
        };
        let state = wt.to_state();
        assert_eq!(state.agent_session_status.as_deref(), Some("waiting"));
    }

    #[test]
    fn to_state_none_agent_session_status() {
        let wt = Worktree {
            id: "test-1".to_string(),
            branch: "swarm/test-1".to_string(),
            prompt: "fix bug".to_string(),
            agent_kind: AgentKind::Claude,
            repo_path: PathBuf::from("/tmp/repo"),
            worktree_path: PathBuf::from("/tmp/wt"),
            created_at: Local::now(),
            agent: None,
            terminals: vec![],
            pr: None,
            phase: WorkerPhase::Running,
            summary: None,

            agent_session_status: None,
        };
        let state = wt.to_state();
        assert!(state.agent_session_status.is_none());
    }

    // --- match_pr_by_local_branches tests ---

    fn sample_prs() -> Vec<serde_json::Value> {
        serde_json::from_str(r#"[
            {"number": 42, "title": "Add README", "state": "OPEN", "url": "https://github.com/org/repo/pull/42", "headRefName": "add-readme"},
            {"number": 43, "title": "Fix bug", "state": "OPEN", "url": "https://github.com/org/repo/pull/43", "headRefName": "fix/login-bug"},
            {"number": 44, "title": "Refactor", "state": "MERGED", "url": "https://github.com/org/repo/pull/44", "headRefName": "refactor-auth"}
        ]"#).unwrap()
    }

    fn no_tips() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn strategy3_finds_pr_by_local_branch() {
        let prs = sample_prs();
        let branches = vec!["main", "fix/login-bug"];
        let result = match_pr_by_local_branches(&branches, &prs, &no_tips());
        let pr = result.expect("should find PR #43");
        assert_eq!(pr["number"].as_u64().unwrap(), 43);
        assert_eq!(pr["title"].as_str().unwrap(), "Fix bug");
        assert_eq!(pr["headRefName"].as_str().unwrap(), "fix/login-bug");
    }

    #[test]
    fn strategy3_returns_none_when_no_branch_matches() {
        let prs = sample_prs();
        let branches = vec!["main", "develop", "feature/unrelated"];
        let result = match_pr_by_local_branches(&branches, &prs, &no_tips());
        assert!(result.is_none());
    }

    #[test]
    fn strategy3_handles_empty_branch_list() {
        let prs = sample_prs();
        let branches: Vec<&str> = vec![];
        let result = match_pr_by_local_branches(&branches, &prs, &no_tips());
        assert!(result.is_none());
    }

    #[test]
    fn strategy3_handles_empty_pr_list() {
        let prs: Vec<serde_json::Value> = vec![];
        let branches = vec!["add-readme", "fix/login-bug"];
        let result = match_pr_by_local_branches(&branches, &prs, &no_tips());
        assert!(result.is_none());
    }

    #[test]
    fn strategy3_multiple_branches_picks_only_open_pr() {
        let prs = sample_prs();
        // Both add-readme and refactor-auth are local — refactor-auth is MERGED so only #42 matches
        let branches = vec!["add-readme", "refactor-auth"];
        let result = match_pr_by_local_branches(&branches, &prs, &no_tips());
        let pr = result.expect("should find PR #42");
        assert_eq!(pr["number"].as_u64().unwrap(), 42);
        assert_eq!(pr["headRefName"].as_str().unwrap(), "add-readme");
    }

    #[test]
    fn strategy3_prefers_highest_pr_number() {
        // When multiple OPEN PRs match local branches, pick the highest number
        let prs: Vec<serde_json::Value> = serde_json::from_str(r#"[
            {"number": 354, "title": "Off-task PR", "state": "OPEN", "url": "https://github.com/org/repo/pull/354", "headRefName": "swarm/off-task-1"},
            {"number": 355, "title": "Actual work", "state": "OPEN", "url": "https://github.com/org/repo/pull/355", "headRefName": "swarm/real-task-1"},
            {"number": 100, "title": "Old PR", "state": "MERGED", "url": "https://github.com/org/repo/pull/100", "headRefName": "swarm/old-1"}
        ]"#).unwrap();
        let branches = vec!["swarm/off-task-1", "swarm/real-task-1", "swarm/old-1"];
        let result = match_pr_by_local_branches(&branches, &prs, &no_tips());
        let pr = result.expect("should find PR #355");
        assert_eq!(pr["number"].as_u64().unwrap(), 355);
        assert_eq!(pr["title"].as_str().unwrap(), "Actual work");
    }

    #[test]
    fn strategy3_commit_match_beats_higher_number() {
        // PR #354 has a commit that matches local branch tip — should win over #355
        let prs: Vec<serde_json::Value> = serde_json::from_str(r#"[
            {"number": 354, "title": "Correct PR", "state": "OPEN", "url": "https://github.com/org/repo/pull/354", "headRefName": "swarm/task-a", "headRefOid": "abc1234def"},
            {"number": 355, "title": "Stale PR", "state": "OPEN", "url": "https://github.com/org/repo/pull/355", "headRefName": "swarm/task-b", "headRefOid": "fff9999aaa"}
        ]"#).unwrap();
        let branches = vec!["swarm/task-a", "swarm/task-b"];
        let tips: HashMap<String, String> = [
            ("swarm/task-a".into(), "abc1234".into()), // matches PR #354's headRefOid prefix
            ("swarm/task-b".into(), "0000000".into()), // does NOT match PR #355
        ].into();
        let result = match_pr_by_local_branches(&branches, &prs, &tips);
        let pr = result.expect("should find PR #354 via commit match");
        assert_eq!(pr["number"].as_u64().unwrap(), 354);
    }

    #[test]
    fn strategy3_updated_at_breaks_tie_when_no_commit_info() {
        // Neither PR has commit info, but #354 was updated more recently
        let prs: Vec<serde_json::Value> = serde_json::from_str(r#"[
            {"number": 355, "title": "Older update", "state": "OPEN", "url": "u/355", "headRefName": "swarm/b", "updatedAt": "2026-02-25T10:00:00Z"},
            {"number": 354, "title": "Newer update", "state": "OPEN", "url": "u/354", "headRefName": "swarm/a", "updatedAt": "2026-02-26T10:00:00Z"}
        ]"#).unwrap();
        let branches = vec!["swarm/a", "swarm/b"];
        let result = match_pr_by_local_branches(&branches, &prs, &no_tips());
        let pr = result.expect("should find PR #354 via updatedAt");
        assert_eq!(pr["number"].as_u64().unwrap(), 354);
    }

    #[test]
    fn strategy3_skips_merged_pr() {
        let prs = sample_prs();
        // Only refactor-auth is local — PR #44 is MERGED, should be skipped
        let branches = vec!["refactor-auth"];
        let result = match_pr_by_local_branches(&branches, &prs, &no_tips());
        assert!(result.is_none(), "should not match merged PRs");
    }

    #[test]
    fn strategy3_only_matches_open_prs() {
        // Verify strategy 3 only matches OPEN PRs, not MERGED/CLOSED
        let prs = sample_prs();
        // add-readme is OPEN, refactor-auth is MERGED
        let branches = vec!["add-readme", "refactor-auth"];
        let result = match_pr_by_local_branches(&branches, &prs, &no_tips());
        let pr = result.expect("should find OPEN PR #42");
        assert_eq!(pr["number"].as_u64().unwrap(), 42);
        assert_eq!(pr["state"].as_str().unwrap(), "OPEN");
    }

    #[test]
    fn strategy3_skips_empty_head_ref() {
        let prs: Vec<serde_json::Value> = serde_json::from_str(
            r#"[{"number": 1, "title": "t", "state": "OPEN", "url": "u", "headRefName": ""}]"#,
        )
        .unwrap();
        let branches = vec!["", "main"];
        let result = match_pr_by_local_branches(&branches, &prs, &no_tips());
        assert!(result.is_none());
    }

    #[test]
    fn strategy3_skips_missing_head_ref() {
        let prs: Vec<serde_json::Value> =
            serde_json::from_str(r#"[{"number": 1, "title": "t", "state": "OPEN", "url": "u"}]"#)
                .unwrap();
        let branches = vec!["main"];
        let result = match_pr_by_local_branches(&branches, &prs, &no_tips());
        assert!(result.is_none());
    }

    // --- Branch name generation tests ---

    #[test]
    fn test_branch_name_sanitizes_spaces() {
        let name = generate_branch_name("Fix the bug", 1);
        assert_eq!(name, "swarm/fix-the-bug-1");
    }

    #[test]
    fn test_branch_name_truncates_long_prompts() {
        let long_prompt = "a]".repeat(25); // 50 chars, should truncate to 40
        let name = generate_branch_name(&long_prompt, 1);
        // sanitize truncates to 40 chars, then we append -1
        assert!(name.starts_with("swarm/"));
        let without_prefix = &name["swarm/".len()..];
        let without_suffix = without_prefix
            .strip_suffix("-1")
            .expect("should end with -1");
        assert!(
            without_suffix.len() <= 40,
            "sanitized part should be <= 40 chars, got {}",
            without_suffix.len()
        );
    }

    #[test]
    fn test_branch_name_removes_special_chars() {
        let name = generate_branch_name("add user auth (v2)", 3);
        assert_eq!(name, "swarm/add-user-auth--v2-3");
        // No parens or spaces in the output
        assert!(!name.contains('('));
        assert!(!name.contains(')'));
        assert!(!name.contains(' '));
    }

    #[test]
    fn test_branch_name_appends_unique_suffix() {
        let name1 = generate_branch_name("same prompt", 1);
        let name2 = generate_branch_name("same prompt", 2);
        assert_ne!(name1, name2);
        assert!(name1.ends_with("-1"));
        assert!(name2.ends_with("-2"));
    }

    #[test]
    fn test_worktree_dir_name_matches_branch() {
        let branch = generate_branch_name("Fix the bug", 1);
        let dir = generate_worktree_dir_name("Fix the bug", 1);
        // Branch should be "swarm/" + dir
        assert_eq!(branch, format!("swarm/{}", dir));
    }

    // --- Agent status file reading tests ---

    #[test]
    fn test_agent_status_none_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let result = read_agent_status(dir.path(), "nonexistent-worker");
        assert!(result.is_none());
    }

    #[test]
    fn test_agent_status_waiting_parsed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("worker-1"),
            "waiting
",
        )
        .unwrap();
        let result = read_agent_status(dir.path(), "worker-1");
        assert_eq!(result.as_deref(), Some("waiting"));
    }

    #[test]
    fn test_agent_status_running_parsed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("worker-1"),
            "running
",
        )
        .unwrap();
        let result = read_agent_status(dir.path(), "worker-1");
        assert_eq!(result.as_deref(), Some("running"));
    }

    #[test]
    fn test_agent_status_unknown_value_handled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("worker-1"),
            "some-unknown-state
",
        )
        .unwrap();
        let result = read_agent_status(dir.path(), "worker-1");
        // Gracefully returns whatever is in the file, trimmed
        assert_eq!(result.as_deref(), Some("some-unknown-state"));
    }

    #[test]
    fn test_agent_status_empty_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("worker-1"),
            "  
",
        )
        .unwrap();
        let result = read_agent_status(dir.path(), "worker-1");
        assert!(result.is_none());
    }

    // --- Repo matching tests ---

    #[test]
    fn test_find_repo_by_name_found() {
        let repos = vec![
            PathBuf::from("/workspace/hive"),
            PathBuf::from("/workspace/swarm"),
            PathBuf::from("/workspace/common"),
        ];
        let result = find_repo_by_name(&repos, "swarm");
        assert_eq!(result, Some(&PathBuf::from("/workspace/swarm")));
    }

    #[test]
    fn test_find_repo_by_name_not_found() {
        let repos = vec![
            PathBuf::from("/workspace/hive"),
            PathBuf::from("/workspace/swarm"),
        ];
        let result = find_repo_by_name(&repos, "nonexistent");
        assert!(result.is_none());
    }

    #[test]
    fn test_find_repo_by_name_empty_repos() {
        let repos: Vec<PathBuf> = vec![];
        let result = find_repo_by_name(&repos, "anything");
        assert!(result.is_none());
    }

    // --- IPC dispatch tests ---
    //
    // These test handle_inbox_message by constructing a minimal App
    // without tmux or socket dependencies. Tests that require actual
    // worktree creation (tmux + git) are deferred until CommandRunner
    // is wired through App (see core/runner.rs).

    /// Build a minimal App for testing (no tmux, no socket listener).
    fn test_app(work_dir: PathBuf, repos: Vec<PathBuf>) -> App {
        let (summary_tx, summary_rx) = mpsc::unbounded_channel();
        let (_inbox_tx, inbox_rx) = mpsc::unbounded_channel();

        App {
            work_dir,
            repos,
            default_agent: AgentKind::ClaudeTui,
            session_name: "test-session".to_string(),
            worktrees: Vec::new(),
            selected: 0,
            mode: Mode::Normal,
            input_buffer: String::new(),
            input_cursor: 0,
            input_label: String::new(),
            agent_select_index: 0,
            repo_select_index: 0,
            pending_action: None,
            confirm_message: String::new(),
            status_message: None,
            show_help: false,
            tick_count: 0,
            sidebar_pane_id: None,
            list_scroll: Cell::new(0),
            prev_selected: None,
            layout_dirty: false,
            pane_style_dirty: false,
            last_refresh: Instant::now(),
            last_pr_check: Instant::now(),
            last_inbox_check: Instant::now(),
            last_inbox_pos: 0,
            summary_tx,
            summary_rx,
            inbox_rx,
            _socket_handle: None,
            is_startup_relaunch: false,
            startup_relaunched: std::collections::HashSet::new(),
        }
    }

    fn make_test_worktree(id: &str, agent_kind: AgentKind) -> Worktree {
        Worktree {
            id: id.to_string(),
            branch: format!("swarm/{}", id),
            prompt: "test task".to_string(),
            agent_kind,
            repo_path: PathBuf::from("/tmp/repo"),
            worktree_path: PathBuf::from("/tmp/wt"),
            created_at: Local::now(),
            agent: None,
            terminals: vec![],
            pr: None,
            phase: WorkerPhase::Running,
            summary: None,

            agent_session_status: None,
        }
    }

    #[test]
    fn test_ipc_create_unknown_repo_no_crash() {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path().to_path_buf();
        std::fs::create_dir_all(work_dir.join(".swarm")).unwrap();

        let repo_a = work_dir.join("repo-a");
        let repo_b = work_dir.join("repo-b");
        let mut app = test_app(work_dir.clone(), vec![repo_a, repo_b]);

        let msg = ipc::InboxMessage::Create {
            id: "msg-1".to_string(),
            prompt: "fix something".to_string(),
            agent: "claude".to_string(),
            repo: Some("nonexistent-repo".to_string()),
            start_point: None,
            timestamp: Local::now(),
        };

        // Should not panic
        app.handle_inbox_message(msg);

        // Should set a flash message about the error
        let (flash, _) = app
            .status_message
            .as_ref()
            .expect("should have flash message");
        assert!(flash.contains("create failed"), "flash: {}", flash);
        assert!(flash.contains("nonexistent-repo"), "flash: {}", flash);

        // Should have emitted a CreateFailed event
        let events_file = work_dir.join(".swarm").join("events.jsonl");
        let content = std::fs::read_to_string(&events_file).unwrap();
        assert!(content.contains("create_failed"));
        assert!(content.contains("nonexistent-repo"));

        // No worktrees added
        assert!(app.worktrees.is_empty());
    }

    #[test]
    fn test_ipc_create_missing_repo_with_multiple_repos() {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path().to_path_buf();
        std::fs::create_dir_all(work_dir.join(".swarm")).unwrap();

        let repo_a = work_dir.join("alpha");
        let repo_b = work_dir.join("beta");
        let mut app = test_app(work_dir.clone(), vec![repo_a, repo_b]);

        // Create without specifying --repo when multiple repos exist
        let msg = ipc::InboxMessage::Create {
            id: "msg-1".to_string(),
            prompt: "fix something".to_string(),
            agent: "claude".to_string(),
            repo: None,
            start_point: None,
            timestamp: Local::now(),
        };

        app.handle_inbox_message(msg);

        // Should fail with --repo required error
        let (flash, _) = app
            .status_message
            .as_ref()
            .expect("should have flash message");
        assert!(flash.contains("create failed"), "flash: {}", flash);
        assert!(flash.contains("--repo required"), "flash: {}", flash);

        assert!(app.worktrees.is_empty());
    }

    #[test]
    fn test_ipc_send_delivers_to_claude_tui_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path().to_path_buf();
        std::fs::create_dir_all(work_dir.join(".swarm")).unwrap();

        let mut app = test_app(work_dir.clone(), vec![]);
        app.worktrees
            .push(make_test_worktree("hive-1", AgentKind::ClaudeTui));

        let msg = ipc::InboxMessage::Send {
            id: "msg-1".to_string(),
            worktree: "hive-1".to_string(),
            message: "please review the PR".to_string(),
            timestamp: Local::now(),
        };

        app.handle_inbox_message(msg);

        // For ClaudeTui agents, the message is written to the per-agent inbox
        let (messages, _) = ipc::read_agent_inbox(&work_dir, "hive-1", 0).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message, "please review the PR");
    }

    #[test]
    fn test_ipc_send_unknown_worktree_handled() {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path().to_path_buf();

        let mut app = test_app(work_dir, vec![]);

        let msg = ipc::InboxMessage::Send {
            id: "msg-1".to_string(),
            worktree: "nonexistent-99".to_string(),
            message: "this goes nowhere".to_string(),
            timestamp: Local::now(),
        };

        // Should not panic
        app.handle_inbox_message(msg);
        assert!(app.worktrees.is_empty());
    }

    #[test]
    fn test_ipc_close_removes_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path().to_path_buf();
        std::fs::create_dir_all(work_dir.join(".swarm")).unwrap();

        let mut app = test_app(work_dir.clone(), vec![]);
        app.worktrees
            .push(make_test_worktree("hive-1", AgentKind::Claude));
        app.worktrees
            .push(make_test_worktree("hive-2", AgentKind::Claude));
        assert_eq!(app.worktrees.len(), 2);

        let msg = ipc::InboxMessage::Close {
            id: "msg-1".to_string(),
            worktree: "hive-1".to_string(),
            timestamp: Local::now(),
        };

        app.handle_inbox_message(msg);

        // hive-1 should be removed, hive-2 should remain
        assert_eq!(app.worktrees.len(), 1);
        assert_eq!(app.worktrees[0].id, "hive-2");

        // Should have emitted a close event
        let events_file = work_dir.join(".swarm").join("events.jsonl");
        let content = std::fs::read_to_string(&events_file).unwrap();
        assert!(content.contains("worktree_closed"));
        assert!(content.contains("hive-1"));
    }

    #[test]
    fn test_ipc_close_unknown_worktree_no_crash() {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path().to_path_buf();

        let mut app = test_app(work_dir, vec![]);
        app.worktrees
            .push(make_test_worktree("hive-1", AgentKind::Claude));

        let msg = ipc::InboxMessage::Close {
            id: "msg-1".to_string(),
            worktree: "nonexistent-99".to_string(),
            timestamp: Local::now(),
        };

        // Should not panic, and should not remove the existing worktree
        app.handle_inbox_message(msg);
        assert_eq!(app.worktrees.len(), 1);
        assert_eq!(app.worktrees[0].id, "hive-1");
    }

    #[test]
    fn test_ipc_merge_unknown_worktree_no_crash() {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path().to_path_buf();

        let mut app = test_app(work_dir, vec![]);
        app.worktrees
            .push(make_test_worktree("hive-1", AgentKind::Claude));

        let msg = ipc::InboxMessage::Merge {
            id: "msg-1".to_string(),
            worktree: "nonexistent-99".to_string(),
            timestamp: Local::now(),
        };

        // Should not panic, and should not affect the existing worktree
        app.handle_inbox_message(msg);
        assert_eq!(app.worktrees.len(), 1);
        assert_eq!(app.worktrees[0].id, "hive-1");
    }

    #[test]
    fn test_ipc_create_valid_single_repo_reaches_creation() {
        // When a single repo exists and no --repo is specified, the
        // Create handler should resolve the repo and attempt worktree
        // creation. Since there's no real tmux/git in the tempdir,
        // create_worktree_with_agent will fail — but the error should
        // be from the creation phase, NOT from repo matching.
        //
        // Once CommandRunner is wired through App this can be replaced
        // with a full happy-path test (see core/runner.rs).
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path().to_path_buf();
        std::fs::create_dir_all(work_dir.join(".swarm")).unwrap();

        let repo = work_dir.join("my-repo");
        std::fs::create_dir_all(&repo).unwrap();
        let mut app = test_app(work_dir.clone(), vec![repo]);

        let msg = ipc::InboxMessage::Create {
            id: "msg-1".to_string(),
            prompt: "add feature".to_string(),
            agent: "claude".to_string(),
            repo: None, // should default to the single repo
            start_point: None,
            timestamp: Local::now(),
        };

        app.handle_inbox_message(msg);

        // The error should be from the creation phase (tmux/git), not
        // from repo matching. "inbox create error" comes from the
        // create_worktree_with_agent error path; "create failed:" with
        // "unknown repo" would mean repo matching failed.
        let (flash, _) = app
            .status_message
            .as_ref()
            .expect("should have flash message from creation failure");
        assert!(
            flash.contains("inbox create error"),
            "expected creation-phase error, got repo-matching error: {}",
            flash
        );
        assert!(
            !flash.contains("unknown repo"),
            "should not fail at repo matching: {}",
            flash
        );
    }

    #[test]
    fn test_ipc_create_valid_named_repo_reaches_creation() {
        // Like the above test, but with an explicit --repo name.
        // Repos must be real git repos for git::repo_name() to resolve.
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path().to_path_buf();
        std::fs::create_dir_all(work_dir.join(".swarm")).unwrap();

        let repo_a = work_dir.join("alpha");
        let repo_b = work_dir.join("beta");
        std::fs::create_dir_all(&repo_a).unwrap();
        std::fs::create_dir_all(&repo_b).unwrap();
        // git init so repo_name() returns the directory name
        let _ = std::process::Command::new("git")
            .args(["init"])
            .current_dir(&repo_a)
            .output();
        let _ = std::process::Command::new("git")
            .args(["init"])
            .current_dir(&repo_b)
            .output();

        let mut app = test_app(work_dir.clone(), vec![repo_a, repo_b]);

        let msg = ipc::InboxMessage::Create {
            id: "msg-1".to_string(),
            prompt: "fix tests".to_string(),
            agent: "claude".to_string(),
            repo: Some("beta".to_string()),
            start_point: None,
            timestamp: Local::now(),
        };

        app.handle_inbox_message(msg);

        // Should reach creation phase, not fail at repo matching
        let (flash, _) = app
            .status_message
            .as_ref()
            .expect("should have flash message from creation failure");
        assert!(
            !flash.contains("unknown repo"),
            "should not fail at repo matching: {}",
            flash
        );
    }

    #[test]
    fn test_ipc_file_inbox_round_trip_dispatch() {
        // End-to-end: write messages to inbox.jsonl, read them, dispatch them.
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path().to_path_buf();
        std::fs::create_dir_all(work_dir.join(".swarm")).unwrap();

        // Write a Send message to the inbox file
        let msg = ipc::InboxMessage::Send {
            id: "msg-1".to_string(),
            worktree: "hive-1".to_string(),
            message: "hello from file inbox".to_string(),
            timestamp: Local::now(),
        };
        ipc::write_inbox(&work_dir, &msg).unwrap();

        // Read from file inbox
        let (messages, new_pos) = ipc::read_inbox(&work_dir, 0).unwrap();
        assert_eq!(messages.len(), 1);
        assert!(new_pos > 0);

        // Dispatch through App
        let mut app = test_app(work_dir.clone(), vec![]);
        app.worktrees
            .push(make_test_worktree("hive-1", AgentKind::ClaudeTui));

        for m in messages {
            app.handle_inbox_message(m);
        }

        // Verify the message was delivered to the agent inbox
        let (agent_msgs, _) = ipc::read_agent_inbox(&work_dir, "hive-1", 0).unwrap();
        assert_eq!(agent_msgs.len(), 1);
        assert_eq!(agent_msgs[0].message, "hello from file inbox");
    }

    // --- poll_agent_statuses tests ---

    #[test]
    fn test_poll_agent_statuses_detects_waiting_transition() {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path().to_path_buf();
        let status_dir = work_dir.join(".swarm").join("agent-status");
        std::fs::create_dir_all(&status_dir).unwrap();

        let mut app = test_app(work_dir, vec![]);
        app.worktrees
            .push(make_test_worktree("hive-1", AgentKind::ClaudeTui));
        assert!(app.worktrees[0].agent_session_status.is_none());

        // Write "running" status
        std::fs::write(status_dir.join("hive-1"), "running\n").unwrap();
        app.poll_agent_statuses();
        assert_eq!(
            app.worktrees[0].agent_session_status.as_deref(),
            Some("running")
        );

        // Transition to "waiting"
        std::fs::write(status_dir.join("hive-1"), "waiting\n").unwrap();
        app.poll_agent_statuses();
        assert_eq!(
            app.worktrees[0].agent_session_status.as_deref(),
            Some("waiting")
        );
    }

    #[test]
    fn test_poll_agent_statuses_no_crash_missing_status_dir() {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path().to_path_buf();
        // Don't create .swarm/agent-status — should handle gracefully

        let mut app = test_app(work_dir, vec![]);
        app.worktrees
            .push(make_test_worktree("hive-1", AgentKind::ClaudeTui));

        // Should not panic
        app.poll_agent_statuses();
        assert!(app.worktrees[0].agent_session_status.is_none());
    }

    #[test]
    fn test_poll_agent_statuses_re_checks_pr_on_waiting_even_if_pr_exists() {
        // Verifies that when an agent transitions to "waiting", the PR
        // lookup fires even if a PR was already known. Before the fix,
        // the `wt.pr.is_none()` guard would skip this.
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path().to_path_buf();
        let status_dir = work_dir.join(".swarm").join("agent-status");
        std::fs::create_dir_all(&status_dir).unwrap();

        let mut app = test_app(work_dir, vec![]);
        let mut wt = make_test_worktree("hive-1", AgentKind::ClaudeTui);
        // Pre-populate with an OPEN PR (simulating 30s poll found it)
        wt.pr = Some(PrInfo {
            number: 42,
            title: "Add feature".to_string(),
            state: "OPEN".to_string(),
            url: "https://github.com/org/repo/pull/42".to_string(),
        });
        wt.agent_session_status = Some("running".to_string());
        // Backdate so the age guard (< 60s) doesn't skip the PR lookup
        wt.created_at = Local::now() - chrono::TimeDelta::seconds(120);
        app.worktrees.push(wt);

        // Agent transitions to "waiting" (e.g., after PR was merged)
        std::fs::write(status_dir.join("hive-1"), "waiting\n").unwrap();
        app.poll_agent_statuses();

        // Status should be updated to "waiting"
        assert_eq!(
            app.worktrees[0].agent_session_status.as_deref(),
            Some("waiting")
        );

        // The PR lookup was attempted. Since there's no real git/gh in
        // the tempdir, lookup_pr_for_worktree will clear the PR (all
        // strategies fail). This proves the lookup ran — before the fix,
        // pr.is_some() would have skipped the lookup and the PR would
        // remain unchanged.
        assert!(
            app.worktrees[0].pr.is_none(),
            "PR should be cleared by lookup attempt (proves lookup ran despite existing PR)"
        );
    }

    #[test]
    fn test_poll_agent_statuses_no_lookup_when_not_newly_waiting() {
        // If the agent was already "waiting" and stays "waiting", no
        // PR lookup should fire (no transition).
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path().to_path_buf();
        let status_dir = work_dir.join(".swarm").join("agent-status");
        std::fs::create_dir_all(&status_dir).unwrap();

        let mut app = test_app(work_dir, vec![]);
        let mut wt = make_test_worktree("hive-1", AgentKind::ClaudeTui);
        wt.pr = Some(PrInfo {
            number: 42,
            title: "Add feature".to_string(),
            state: "OPEN".to_string(),
            url: "https://github.com/org/repo/pull/42".to_string(),
        });
        // Already "waiting" from a previous poll
        wt.agent_session_status = Some("waiting".to_string());
        app.worktrees.push(wt);

        // Status file still says "waiting" — no transition
        std::fs::write(status_dir.join("hive-1"), "waiting\n").unwrap();
        app.poll_agent_statuses();

        // PR should remain unchanged (no lookup attempted)
        assert!(
            app.worktrees[0].pr.is_some(),
            "PR should remain untouched when no status transition occurred"
        );
        assert_eq!(app.worktrees[0].pr.as_ref().unwrap().state, "OPEN");
    }

    // ── Phase transition tests ─────────────────────────────

    #[test]
    fn to_state_computes_status_from_phase() {
        let mut wt = make_test_worktree("test-1", AgentKind::Claude);

        // Active phases → "running" status
        for phase in &[
            WorkerPhase::Creating,
            WorkerPhase::Starting,
            WorkerPhase::Running,
            WorkerPhase::Waiting,
        ] {
            wt.phase = phase.clone();
            let state = wt.to_state();
            assert_eq!(
                state.status, "running",
                "phase {:?} should produce status=running",
                phase
            );
        }

        // Terminal phases → "done" status
        for phase in &[WorkerPhase::Completed, WorkerPhase::Failed] {
            wt.phase = phase.clone();
            let state = wt.to_state();
            assert_eq!(
                state.status, "done",
                "phase {:?} should produce status=done",
                phase
            );
        }
    }

    #[test]
    fn to_state_preserves_phase() {
        let mut wt = make_test_worktree("test-1", AgentKind::Claude);
        wt.phase = WorkerPhase::Waiting;
        let state = wt.to_state();
        assert_eq!(state.phase, WorkerPhase::Waiting);
    }

    #[test]
    fn from_state_restores_phase() {
        let ws = state::WorktreeState {
            id: "test-1".to_string(),
            branch: "swarm/test-1".to_string(),
            prompt: "test".to_string(),
            agent_kind: AgentKind::Claude,
            repo_path: PathBuf::from("/tmp/repo"),
            worktree_path: PathBuf::from("/tmp/wt"),
            created_at: Local::now(),
            agent: None,
            terminals: vec![],
            summary: None,
            pr: None,
            phase: WorkerPhase::Waiting,
            status: "running".to_string(),
            agent_session_status: None,
        };
        let wt = Worktree::from_state(&ws);
        assert_eq!(wt.phase, WorkerPhase::Waiting);
    }

    #[test]
    fn poll_agent_statuses_transitions_to_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path().to_path_buf();
        let status_dir = work_dir.join(".swarm").join("agent-status");
        std::fs::create_dir_all(&status_dir).unwrap();

        let mut app = test_app(work_dir, vec![]);
        let mut wt = make_test_worktree("hive-1", AgentKind::ClaudeTui);
        wt.phase = WorkerPhase::Running;
        app.worktrees.push(wt);

        std::fs::write(status_dir.join("hive-1"), "waiting\n").unwrap();
        app.poll_agent_statuses();

        assert_eq!(app.worktrees[0].phase, WorkerPhase::Waiting);
    }

    #[test]
    fn poll_agent_statuses_transitions_back_to_running() {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path().to_path_buf();
        let status_dir = work_dir.join(".swarm").join("agent-status");
        std::fs::create_dir_all(&status_dir).unwrap();

        let mut app = test_app(work_dir, vec![]);
        let mut wt = make_test_worktree("hive-1", AgentKind::ClaudeTui);
        wt.phase = WorkerPhase::Waiting;
        wt.agent_session_status = Some("waiting".to_string());
        app.worktrees.push(wt);

        std::fs::write(status_dir.join("hive-1"), "running\n").unwrap();
        app.poll_agent_statuses();

        assert_eq!(app.worktrees[0].phase, WorkerPhase::Running);
    }
}
