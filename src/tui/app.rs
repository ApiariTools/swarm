#![allow(dead_code)]

use crate::core::{agent::AgentKind, git, ipc, merge, state, tmux};
use chrono::{DateTime, Local};
use color_eyre::Result;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

/// Pane foreground colors for selected/dimmed states.
const PANE_FG_SELECTED: &str = "#dcdce1"; // full FROST brightness
const PANE_FG_DIMMED: &str = "#6e6b65"; // ~40% dimmed
const PANE_BG: &str = "#282520"; // COMB (unchanged for both)

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
#[derive(Debug, Clone)]
pub struct PrInfo {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub url: String,
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
    /// Prompt to send once the agent is ready (sent after a delay, then cleared).
    pub pending_prompt: Option<(String, Instant)>,
}

impl Worktree {
    /// Convert to persistable state.
    fn to_state(&self) -> state::WorktreeState {
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
            pr: None,
            pending_prompt: None,
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
    prev_selected: Option<usize>,
    last_refresh: Instant,
    last_pr_check: Instant,
    last_inbox_check: Instant,
    last_inbox_pos: u64,
}

impl App {
    pub fn new(work_dir: PathBuf, agent: String) -> Result<Self> {
        let repos = git::detect_repos(&work_dir)?;
        let default_agent = AgentKind::from_str(&agent).unwrap_or(AgentKind::Claude);

        // Derive session name from dir
        let dir_name = work_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "swarm".to_string());
        let session_name = format!("swarm-{}", dir_name);

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
            prev_selected: None,
            last_refresh: Instant::now(),
            last_pr_check: Instant::now(),
            last_inbox_check: Instant::now(),
            last_inbox_pos: 0,
        };

        // Restore previous session
        app.restore_state();

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

                    if let Some(ref mut agent) = wt.agent {
                        if !live_pane_ids.contains(&agent.pane_id) {
                            agent.status = PaneStatus::Done;
                        }
                    }
                    for term in &mut wt.terminals {
                        if !live_pane_ids.contains(&term.pane_id) {
                            term.status = PaneStatus::Done;
                        }
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

        // Truncate inbox — everything up to last_inbox_pos has been processed.
        let inbox_path = self.work_dir.join(".swarm").join("inbox.jsonl");
        if inbox_path.exists() {
            let _ = std::fs::write(&inbox_path, b"");
            self.last_inbox_pos = 0;
        }

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
                        pending_prompt: None,
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
            // Re-apply colors to all live panes
            for i in 0..total {
                self.apply_worktree_color(i);
            }
            self.update_pane_selection();
            self.flash(format!(
                "restored {} worktree{}",
                total,
                if total == 1 { "" } else { "s" }
            ));
        }
    }

    pub fn save_state(&self) {
        let swarm_state = state::SwarmState {
            session_name: self.session_name.clone(),
            sidebar_pane_id: self.sidebar_pane_id.clone(),
            worktrees: self.worktrees.iter().map(|w| w.to_state()).collect(),
            last_inbox_pos: self.last_inbox_pos,
        };

        let _ = state::save_state(&self.work_dir, &swarm_state);
    }

    // ── Navigation ─────────────────────────────────────────

    pub fn select_next(&mut self) {
        if self.worktrees.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.worktrees.len();
        self.update_pane_selection();
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
        self.update_pane_selection();
    }

    // ── Jump to Agent Pane ─────────────────────────────────

    pub fn jump_to_selected(&mut self) {
        if self.worktrees.is_empty() {
            return;
        }
        let idx = self.selected;
        let wt = &self.worktrees[idx];

        // If there's a live agent pane, jump to it
        if let Some(ref agent) = wt.agent {
            if agent.status == PaneStatus::Running {
                let _ = tmux::select_pane(&agent.pane_id);
                return;
            }
        }
        // If there's a live terminal pane, jump to it
        if let Some(term) = wt.terminals.iter().find(|t| t.status == PaneStatus::Running) {
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

        // Same smart split logic: horizontal if no live agents, vertical if stacking
        let has_live_agents = self.worktrees.iter().enumerate().any(|(i, w)| {
            i != idx
                && w.agent
                    .as_ref()
                    .is_some_and(|a| a.status == PaneStatus::Running)
        });

        let pane_id = if !has_live_agents {
            let split_from = self
                .sidebar_pane_id
                .clone()
                .unwrap_or_else(|| "%0".to_string());
            tmux::split_pane_horizontal(&split_from, &dir.to_string_lossy(), 70)?
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
            tmux::split_pane_vertical(&last_agent_pane, &dir.to_string_lossy())?
        };

        let _ = tmux::set_pane_title(&pane_id, &wt_id);

        // Launch agent with prompt baked into the command
        let cmd = agent.launch_cmd_with_prompt(&prompt, true);
        tmux::send_keys_to_pane(&pane_id, &cmd)?;

        self.worktrees[idx].agent = Some(TrackedPane {
            pane_id: pane_id.clone(),
            status: PaneStatus::Running,
        });
        self.worktrees[idx].pending_prompt = None;

        // Rebalance, re-apply styling, re-select sidebar (after setting agent so pane is included)
        self.rebalance_layout();
        let _ = tmux::apply_session_style(&self.session_name);
        if let Some(ref sidebar) = self.sidebar_pane_id {
            let _ = tmux::select_pane(sidebar);
        }
        self.apply_worktree_color(idx);
        self.update_pane_selection();
        self.save_state();

        // Emit event
        let _ = ipc::emit_event(
            &self.work_dir,
            &ipc::SwarmEvent::AgentStarted {
                worktree: wt_id,
                pane_id,
                timestamp: Local::now(),
            },
        );

        self.flash(format!("{} relaunched", agent.label()));
        Ok(())
    }

    // ── New Worktree Flow ─────────────────────────────────

    pub fn start_new_worktree(&mut self) {
        let exe = std::env::current_exe()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "swarm".to_string());
        let dir_str = self.work_dir.to_string_lossy();
        let cmd = format!("'{}' -d '{}' pick", exe, dir_str);

        if let Err(e) = tmux::display_popup(
            &self.session_name,
            "60%",
            "50%",
            " new task ",
            &cmd,
        ) {
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
            self.flash(pr.url.clone());
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

        self.apply_worktree_color(idx);
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
        let repo_path = self.repos.get(self.repo_select_index).cloned()
            .unwrap_or_else(|| self.work_dir.clone());

        self.mode = Mode::Normal;
        self.input_buffer.clear();
        self.input_cursor = 0;

        if let Err(e) = self.create_worktree_with_agent(&prompt, agent, &repo_path, None) {
            self.flash(format!("error: {}", e));
        }
    }

    fn create_worktree_with_agent(&mut self, prompt: &str, agent: AgentKind, repo_path: &std::path::Path, start_point: Option<&str>) -> Result<()> {
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

        if let Some(parent) = worktree_dir.parent() {
            std::fs::create_dir_all(parent)?;
        }

        git::create_worktree(&repo_path, &branch_name, &worktree_dir, start_point)?;

        // Auto-trust mise if the repo uses it
        if repo_path.join(".mise.toml").exists() || repo_path.join("mise.toml").exists() {
            let _ = Command::new("mise")
                .arg("trust")
                .current_dir(&worktree_dir)
                .output();
        }

        let window_name = format!("{}-{}", repo_name, num);

        // Smart split: first agent splits horizontal from sidebar (creates right column),
        // subsequent agents split vertical from last agent (stacks in right column).
        let has_live_agents = self.worktrees.iter().any(|w| {
            w.agent
                .as_ref()
                .is_some_and(|a| a.status == PaneStatus::Running)
        });

        let pane_id = if !has_live_agents {
            // First agent: split horizontal from sidebar
            let split_from = self
                .sidebar_pane_id
                .clone()
                .unwrap_or_else(|| "%0".to_string());
            tmux::split_pane_horizontal(
                &split_from,
                &worktree_dir.to_string_lossy(),
                70,
            )?
        } else {
            // Subsequent agents: split vertical from last live agent pane
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
            tmux::split_pane_vertical(
                &last_agent_pane,
                &worktree_dir.to_string_lossy(),
            )?
        };

        // Set pane title
        let _ = tmux::set_pane_title(&pane_id, &window_name);

        // Launch agent with prompt baked into the command
        let cmd = agent.launch_cmd_with_prompt(prompt, true);
        tmux::send_keys_to_pane(&pane_id, &cmd)?;

        self.worktrees.push(Worktree {
            id: window_name.clone(),
            branch: branch_name.clone(),
            prompt: prompt.to_string(),
            agent_kind: agent.clone(),
            repo_path,
            worktree_path: worktree_dir,
            created_at: Local::now(),
            agent: Some(TrackedPane {
                pane_id: pane_id.clone(),
                status: PaneStatus::Running,
            }),
            terminals: Vec::new(),
            pr: None,
            pending_prompt: None,
        });

        self.selected = self.worktrees.len() - 1;
        self.apply_worktree_color(self.selected);
        self.prev_selected = None;
        self.update_pane_selection();

        // Rebalance layout and re-apply styling (after push so the new pane is included)
        self.rebalance_layout();
        let _ = tmux::apply_session_style(&self.session_name);

        // Re-select sidebar pane so TUI keeps focus
        if let Some(ref sidebar) = self.sidebar_pane_id {
            let _ = tmux::select_pane(sidebar);
        }
        self.save_state();

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

        // Remove worktree if it exists
        if wt.worktree_path.exists() {
            let _ = git::remove_worktree(&wt.repo_path, &wt.worktree_path);
            let _ = git::delete_branch(&wt.repo_path, &wt.branch);
        }

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

    fn process_inbox(&mut self) {
        let (messages, new_pos) = match ipc::read_inbox(&self.work_dir, self.last_inbox_pos) {
            Ok(result) => result,
            Err(_) => return,
        };

        if new_pos == self.last_inbox_pos {
            return;
        }
        self.last_inbox_pos = new_pos;

        if messages.is_empty() {
            // Position advanced (e.g. blank lines) — persist it
            self.save_state();
            return;
        }

        for msg in messages {
            match msg {
                ipc::InboxMessage::Create {
                    prompt,
                    agent,
                    repo,
                    start_point,
                    ..
                } => {
                    let agent_kind = AgentKind::from_str(&agent).unwrap_or(AgentKind::Claude);
                    let repo_path = repo
                        .and_then(|name| self.repos.iter().find(|r| git::repo_name(r) == name).cloned())
                        .or_else(|| self.repos.first().cloned())
                        .unwrap_or_else(|| self.work_dir.clone());
                    if let Err(e) = self.create_worktree_with_agent(&prompt, agent_kind, &repo_path, start_point.as_deref()) {
                        self.flash(format!("inbox create error: {}", e));
                    }
                }
                ipc::InboxMessage::Send {
                    worktree, message, ..
                } => {
                    if let Some(wt) = self.worktrees.iter().find(|w| w.id == worktree) {
                        if let Some(ref agent) = wt.agent {
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
    }

    // ── PR Status ──────────────────────────────────────────

    fn refresh_pr_statuses(&mut self) {
        let mut merged_ids = Vec::new();

        for wt in &mut self.worktrees {
            if wt.branch.is_empty() {
                continue;
            }

            let output = Command::new("gh")
                .args([
                    "pr",
                    "list",
                    "--head",
                    &wt.branch,
                    "--state",
                    "all",
                    "--json",
                    "number,title,state,url",
                    "--limit",
                    "1",
                ])
                .current_dir(&wt.repo_path)
                .output();

            if let Ok(output) = output {
                if output.status.success() {
                    let text = String::from_utf8_lossy(&output.stdout);
                    if let Ok(prs) = serde_json::from_str::<Vec<serde_json::Value>>(text.trim()) {
                        if let Some(pr) = prs.first() {
                            let state = pr["state"].as_str().unwrap_or("").to_string();
                            if state == "MERGED" && wt.pr.as_ref().map_or(true, |p| p.state != "MERGED") {
                                merged_ids.push(wt.id.clone());
                            }
                            wt.pr = Some(PrInfo {
                                number: pr["number"].as_u64().unwrap_or(0),
                                title: pr["title"].as_str().unwrap_or("").to_string(),
                                state,
                                url: pr["url"].as_str().unwrap_or("").to_string(),
                            });
                        } else {
                            wt.pr = None;
                        }
                    }
                }
            }
        }

        // Auto-close worktrees whose PRs were just merged
        for id in merged_ids {
            if let Some(idx) = self.worktrees.iter().position(|w| w.id == id) {
                let prompt = self.worktrees[idx].prompt.clone();
                let _ = self.close_worktree(idx);
                self.flash(format!("auto-closed \"{}\" (PR merged)", prompt));
            }
        }
    }

    // ── Tick ───────────────────────────────────────────────

    pub fn tick(&mut self) {
        self.tick_count += 1;

        self.deliver_pending_prompts();

        // Process inbox every 500ms
        if self.last_inbox_check.elapsed().as_millis() >= 500 {
            self.process_inbox();
            self.last_inbox_check = Instant::now();
        }

        // Refresh pane states every 3s
        if self.last_refresh.elapsed().as_secs() >= 3 {
            self.refresh_pane_states();
            self.last_refresh = Instant::now();
        }

        // Refresh PR statuses every 30s
        if self.last_pr_check.elapsed().as_secs() >= 30 {
            self.refresh_pr_statuses();
            self.last_pr_check = Instant::now();
        }
    }

    fn deliver_pending_prompts(&mut self) {
        for wt in &mut self.worktrees {
            if let Some((ref prompt, created)) = wt.pending_prompt {
                if created.elapsed().as_secs() >= 5 {
                    if let Some(ref agent) = wt.agent {
                        let _ = tmux::send_keys_to_pane(&agent.pane_id, prompt);
                    }
                    wt.pending_prompt = None;
                }
            }
        }
    }

    fn refresh_pane_states(&mut self) {
        // Check which panes are still alive
        let session_window = self.session_name.clone();
        let live_pane_ids: Vec<String> = tmux::list_panes(&session_window)
            .unwrap_or_default()
            .iter()
            .map(|p| p.pane_id.clone())
            .collect();

        let mut any_done = false;
        for wt in &mut self.worktrees {
            if let Some(ref mut agent) = wt.agent {
                if agent.status == PaneStatus::Running && !live_pane_ids.contains(&agent.pane_id) {
                    agent.status = PaneStatus::Done;
                    any_done = true;
                }
            }
            for term in &mut wt.terminals {
                if term.status == PaneStatus::Running && !live_pane_ids.contains(&term.pane_id) {
                    term.status = PaneStatus::Done;
                }
            }
        }

        // Emit agent_done events
        if any_done {
            for wt in &self.worktrees {
                if let Some(ref agent) = wt.agent {
                    if agent.status == PaneStatus::Done {
                        let _ = ipc::emit_event(
                            &self.work_dir,
                            &ipc::SwarmEvent::AgentDone {
                                worktree: wt.id.clone(),
                                timestamp: Local::now(),
                            },
                        );
                    }
                }
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

    /// Apply border color to all live panes of a worktree.
    fn apply_worktree_color(&self, idx: usize) {
        let color = self.worktree_border_color(idx);
        if let Some(wt) = self.worktrees.get(idx) {
            if let Some(ref agent) = wt.agent {
                if agent.status == PaneStatus::Running {
                    let _ = tmux::set_pane_color(&agent.pane_id, color);
                }
            }
            for term in &wt.terminals {
                if term.status == PaneStatus::Running {
                    let _ = tmux::set_pane_color(&term.pane_id, color);
                }
            }
        }
    }

    /// Update pane selection styling — dims non-selected worktrees, brightens selected.
    /// Uses delta updates when possible (only touches changed worktrees).
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

        for idx in indices_to_update {
            let is_selected = idx == selected;
            let fg = if is_selected { PANE_FG_SELECTED } else { PANE_FG_DIMMED };
            let style = format!("bg={},fg={}", PANE_BG, fg);

            if let Some(wt) = self.worktrees.get(idx) {
                if let Some(ref agent) = wt.agent {
                    if agent.status == PaneStatus::Running {
                        let _ = tmux::set_pane_style(&agent.pane_id, &style);
                        let _ = tmux::set_pane_selected(&agent.pane_id, is_selected);
                    }
                }
                for term in &wt.terminals {
                    if term.status == PaneStatus::Running {
                        let _ = tmux::set_pane_style(&term.pane_id, &style);
                        let _ = tmux::set_pane_selected(&term.pane_id, is_selected);
                    }
                }
            }
        }

        self.prev_selected = Some(selected);
    }

    /// Rebalance tmux pane layout into a tiled grid.
    fn rebalance_layout(&self) {
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
                if let Some(ref agent) = wt.agent {
                    if agent.status == PaneStatus::Running {
                        panes.push(agent.pane_id.clone());
                    }
                }
                for term in &wt.terminals {
                    if term.status == PaneStatus::Running {
                        panes.push(term.pane_id.clone());
                    }
                }
                panes
            })
            .collect();

        let _ = tmux::apply_tiled_layout(&session_window, &sidebar, 38, pane_groups);
    }

    fn next_worktree_num(&self) -> usize {
        let max = self
            .worktrees
            .iter()
            .filter_map(|w| w.id.rsplit('-').next().and_then(|n| n.parse::<usize>().ok()))
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

/// Sanitize a string for use in branch names.
fn sanitize(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
        .chars()
        .take(40)
        .collect()
}
