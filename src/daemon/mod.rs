pub mod a2a_server;
pub mod agent_supervisor;
pub mod event_logger;
pub mod ipc_client;
pub mod lifecycle;
pub mod managed_agent;
pub mod protocol;
pub mod socket_server;

use crate::core::agent::AgentKind;
use crate::core::agent_card::build_agent_card;
use crate::core::state::PrInfo;
use crate::core::state::{SwarmState, WorkerPhase, WorktreeState};
use crate::core::{git, ipc, state};
use agent_supervisor::SupervisorEvent;
use chrono::Local;
use color_eyre::Result;
use protocol::{A2aTaskMessage, DaemonRequest, DaemonResponse, WorkerInfo, WorkspaceInfo};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::sync::{broadcast, mpsc};

// ── Global PID management ────────────────────────────────

/// Write the current process PID to the global PID file.
fn write_pid() -> Result<()> {
    let path = ipc::global_pid_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, std::process::id().to_string())?;
    Ok(())
}

/// Read the PID from the global PID file.
pub fn read_global_pid() -> Option<u32> {
    std::fs::read_to_string(ipc::global_pid_path())
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Remove the global PID file.
fn remove_pid() {
    let _ = std::fs::remove_file(ipc::global_pid_path());
}

/// Check whether a process is alive.
///
/// Uses `kill(pid, 0)` which checks existence without sending a signal.
/// Returns `true` if the process exists (including when we lack permission
/// to signal it, i.e. `EPERM`).
pub fn is_process_alive(pid: u32) -> bool {
    let ret = unsafe { libc::kill(pid as i32, 0) };
    if ret == 0 {
        return true;
    }
    // EPERM means the process exists but we can't signal it
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

// ── Per-workspace state ──────────────────────────────────

/// State for a single workspace managed by the daemon.
struct WorkspaceState {
    path: PathBuf,
    repos: Vec<PathBuf>,
    workers: HashMap<String, ManagedWorker>,
    inbox_offset: u64,
    /// Whether to auto-close workers when their PR is merged.
    close_on_pr_merge: bool,
}

/// State for a managed worker within the daemon.
struct ManagedWorker {
    id: String,
    branch: String,
    prompt: String,
    kind: AgentKind,
    repo_path: PathBuf,
    worktree_path: PathBuf,
    phase: WorkerPhase,
    session_id: Option<String>,
    restart_count: u32,
    pr: Option<PrInfo>,
    created_at: chrono::DateTime<Local>,
    /// Channel to send messages to the agent supervisor task.
    message_tx: Option<mpsc::UnboundedSender<String>>,
    /// Cached A2A Agent Card, built at creation time.
    agent_card: a2a_types::AgentCard,
    /// Worker role: "worker" or "reviewer".
    role: Option<String>,
    /// PR number being reviewed (when role = "reviewer").
    review_pr: Option<u64>,
    /// Accumulated text output from the agent (used to parse review verdict).
    /// Not persisted to state.json — reconstructed from event log on restart.
    accumulated_text: String,
    /// Parsed review verdict (populated when reviewer worker produces output).
    review_verdict: Option<crate::core::state::ReviewVerdict>,
    /// Branch name signalled ready by the worker (via `BRANCH_READY: <name>`).
    ready_branch: Option<String>,
}

impl ManagedWorker {
    fn to_worker_info(&self) -> WorkerInfo {
        WorkerInfo {
            id: self.id.clone(),
            branch: self.branch.clone(),
            prompt: self.prompt.clone(),
            agent: self.kind.label().to_string(),
            phase: self.phase.clone(),
            session_id: self.session_id.clone(),
            pr_url: self.pr.as_ref().map(|p| p.url.clone()),
            pr_number: self.pr.as_ref().map(|p| p.number),
            pr_title: self.pr.as_ref().map(|p| p.title.clone()),
            pr_state: self.pr.as_ref().map(|p| p.state.clone()),
            restart_count: self.restart_count,
            created_at: Some(self.created_at),
            agent_card: Some(self.agent_card.clone()),
            role: self.role.clone(),
            review_verdict: self.review_verdict.clone(),
        }
    }

    fn to_worktree_state(&self) -> WorktreeState {
        let status = if self.phase.is_active() {
            "running"
        } else {
            "done"
        }
        .to_string();

        let agent_session_status = match self.phase {
            WorkerPhase::Waiting => Some("waiting".to_string()),
            WorkerPhase::Running => Some("running".to_string()),
            _ => None,
        };

        WorktreeState {
            id: self.id.clone(),
            branch: self.branch.clone(),
            prompt: self.prompt.clone(),
            agent_kind: self.kind.clone(),
            repo_path: self.repo_path.clone(),
            worktree_path: self.worktree_path.clone(),
            created_at: self.created_at,
            agent: None, // daemon mode: no agent process
            terminals: vec![],
            summary: None,
            pr: self.pr.clone(),
            phase: self.phase.clone(),
            status,
            agent_session_status,
            agent_pid: None,
            session_id: self.session_id.clone(),
            restart_count: Some(self.restart_count),
            role: self.role.clone(),
            review_pr: self.review_pr,
            review_verdict: self.review_verdict.clone(),
            ready_branch: self.ready_branch.clone(),
        }
    }
}

// ── Daemon lifecycle ─────────────────────────────────────

/// Start the swarm daemon.
pub async fn start(work_dir: PathBuf, foreground: bool, tcp_bind: Option<String>) -> Result<()> {
    // Check if already running (global PID)
    if let Some(pid) = read_global_pid() {
        if is_process_alive(pid) {
            return Err(color_eyre::eyre::eyre!(
                "daemon already running (pid {})",
                pid
            ));
        }
        // Stale PID file
        remove_pid();
    }

    // Generate auth token if TCP is enabled
    let auth_token = tcp_bind.as_ref().map(|_| uuid::Uuid::new_v4().to_string());

    if foreground {
        run_daemon(Some(work_dir), tcp_bind, auth_token).await
    } else {
        println!(
            "Starting swarm daemon in foreground mode. Use a process manager for background operation."
        );
        run_daemon(Some(work_dir), tcp_bind, auth_token).await
    }
}

/// Stop the daemon.
pub fn stop(_work_dir: &Path) -> Result<()> {
    match read_global_pid() {
        Some(pid) if is_process_alive(pid) => {
            tracing::info!(pid, "Stopping daemon");
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            // Wait briefly for it to exit
            for _ in 0..50 {
                if !is_process_alive(pid) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            if is_process_alive(pid) {
                tracing::warn!("Daemon did not exit, sending SIGKILL");
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
            remove_pid();
            Ok(())
        }
        Some(_) => {
            remove_pid();
            Err(color_eyre::eyre::eyre!(
                "daemon not running (stale PID file removed)"
            ))
        }
        None => Err(color_eyre::eyre::eyre!("daemon not running")),
    }
}

/// Restart the daemon.
pub async fn restart(work_dir: PathBuf, foreground: bool, tcp_bind: Option<String>) -> Result<()> {
    let _ = stop(&work_dir); // Ignore errors if not running
    start(work_dir, foreground, tcp_bind).await
}

/// Get daemon status.
pub fn status(_work_dir: &Path) -> Result<()> {
    match read_global_pid() {
        Some(pid) if is_process_alive(pid) => {
            println!("daemon running (pid {})", pid);
            println!("socket: {}", ipc::global_socket_path().display());
            Ok(())
        }
        Some(_) => {
            remove_pid();
            println!("daemon not running (stale PID file removed)");
            Ok(())
        }
        None => {
            println!("daemon not running");
            Ok(())
        }
    }
}

/// Get the current size of a workspace's inbox.jsonl (for seeking to end on startup).
fn inbox_file_size(ws_path: &Path) -> u64 {
    let inbox_path = ws_path.join(".swarm").join("inbox.jsonl");
    std::fs::metadata(&inbox_path).map(|m| m.len()).unwrap_or(0)
}

// ── Workspace registration ───────────────────────────────

/// Register a workspace: canonicalize path, detect repos, load existing state.
async fn register_workspace(
    workspaces: &mut HashMap<PathBuf, WorkspaceState>,
    path: &Path,
) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    if workspaces.contains_key(&canonical) {
        return Ok(canonical);
    }

    let bg_canonical = canonical.clone();
    let repos =
        tokio::task::spawn_blocking(move || git::detect_repos(&bg_canonical).unwrap_or_default())
            .await
            .unwrap_or_default();

    // Load existing workers from state.json
    let mut workers = HashMap::new();
    if let Ok(Some(existing_state)) = state::load_state(&canonical) {
        // Cache the default profile to avoid re-reading from disk per worker.
        // TODO: profile slug is not persisted in state.json yet, so restored
        // workers always use "default". A future PR should add a
        // `profile_slug` field to WorktreeState.
        let default_profile = crate::core::profile::load_profile(&canonical, "default");
        for wt in &existing_state.worktrees {
            if wt.phase.is_active() {
                let profile_for_card = if wt.role.as_deref() == Some("reviewer") {
                    crate::core::profile::REVIEWER_PROFILE.to_string()
                } else {
                    default_profile.clone()
                };
                workers.insert(
                    wt.id.clone(),
                    ManagedWorker {
                        id: wt.id.clone(),
                        branch: wt.branch.clone(),
                        prompt: wt.prompt.clone(),
                        kind: wt.agent_kind.clone(),
                        repo_path: wt.repo_path.clone(),
                        worktree_path: wt.worktree_path.clone(),
                        phase: wt.phase.clone(),
                        session_id: wt.session_id.clone(),
                        restart_count: wt.restart_count.unwrap_or(0),
                        pr: wt.pr.clone(),
                        created_at: wt.created_at,
                        message_tx: None,
                        agent_card: build_agent_card(
                            &wt.id,
                            &git::repo_name(&wt.repo_path),
                            wt.agent_kind.label(),
                            &profile_for_card,
                        ),
                        role: wt.role.clone(),
                        review_pr: wt.review_pr,
                        accumulated_text: String::new(),
                        review_verdict: wt.review_verdict.clone(),
                        ready_branch: wt.ready_branch.clone(),
                    },
                );
            }
        }
    }

    let worker_count = workers.len();
    // Seek to end of inbox so we don't replay old messages on restart
    let inbox_pos = inbox_file_size(&canonical);
    let config = crate::core::config::load_config(&canonical);
    workspaces.insert(
        canonical.clone(),
        WorkspaceState {
            path: canonical.clone(),
            repos,
            workers,
            inbox_offset: inbox_pos,
            close_on_pr_merge: config.close_on_pr_merge,
        },
    );

    tracing::info!(
        path = %canonical.display(),
        repos = workspaces[&canonical].repos.len(),
        workers = worker_count,
        "Registered workspace",
    );

    Ok(canonical)
}

// ── Main daemon loop ─────────────────────────────────────

/// The main daemon event loop.
pub(crate) async fn run_daemon(
    initial_work_dir: Option<PathBuf>,
    tcp_bind: Option<String>,
    auth_token: Option<String>,
) -> Result<()> {
    write_pid()?;
    // Init file logging — _log_guard must stay alive for the process lifetime.
    let _log_guard = initial_work_dir.as_deref().map(crate::core::log::init);

    // Do not mutate process-global environment here. In Rust 2024,
    // `std::env::remove_var` is `unsafe` because it races with other threads.
    // Child agent processes clear Claude-specific vars per-spawn in the SDK
    // transport (`Command::env_remove(...)`), which is the thread-safe path.

    // Ignore SIGPIPE — sockets and other fds may close unexpectedly.
    // tracing-appender writes to a file, but the socket server and agent
    // processes can still trigger SIGPIPE via other fd operations.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    tracing::info!(pid = std::process::id(), "Daemon starting");

    // Set up signal handlers
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    // Event broadcast channel for subscribers — carries full DaemonResponse
    // so both AgentEvent and StateChanged can be pushed to subscribers.
    let (event_tx, _) = broadcast::channel::<DaemonResponse>(1024);

    // Supervisor event channel
    let (supervisor_tx, mut supervisor_rx) = mpsc::unbounded_channel::<SupervisorEvent>();

    // Start the socket server on global path
    let (mut request_rx, _socket_handle) =
        socket_server::start(event_tx.clone(), tcp_bind, auth_token)?;

    // HTTP request channel — A2A HTTP handlers route requests here, and the
    // daemon select! loop processes them alongside Unix socket requests.
    let (http_req_tx, mut http_req_rx) =
        mpsc::unbounded_channel::<(DaemonRequest, mpsc::UnboundedSender<DaemonResponse>)>();

    // Start the A2A HTTP server (port 0 = OS-assigned).
    let (_a2a_handle, a2a_port) = a2a_server::start(0, http_req_tx, event_tx.clone()).await?;

    // Multi-workspace state
    let mut workspaces: HashMap<PathBuf, WorkspaceState> = HashMap::new();

    // State save debounce
    let mut state_dirty = false;
    let mut last_state_save = std::time::Instant::now();
    let state_save_interval = std::time::Duration::from_secs(2);

    // Inbox polling
    let inbox_poll_interval = std::time::Duration::from_millis(500);
    let mut inbox_poll = tokio::time::interval(inbox_poll_interval);
    inbox_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // PR polling (runs in background to avoid blocking the select loop)
    let pr_poll_interval = std::time::Duration::from_secs(30);
    let mut pr_poll = tokio::time::interval(pr_poll_interval);
    pr_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pending_pr_poll: Option<tokio::task::JoinHandle<Vec<PrPollResult>>> = None;

    // Daemon is ready to accept connections immediately.
    tracing::info!("Daemon ready, accepting connections");

    // Register initial workspace in a background task — detect_repos is expensive
    // and we don't want to block the select loop from processing Ping/Subscribe.
    let mut pending_register: Option<tokio::task::JoinHandle<Option<(PathBuf, WorkspaceState)>>> =
        if let Some(ref wd) = initial_work_dir {
            let wd = wd.clone();
            Some(tokio::task::spawn(async move {
                let canonical = std::fs::canonicalize(&wd).unwrap_or_else(|_| wd.to_path_buf());
                let bg = canonical.clone();
                let repos =
                    tokio::task::spawn_blocking(move || git::detect_repos(&bg).unwrap_or_default())
                        .await
                        .unwrap_or_default();
                let mut workers = HashMap::new();
                if let Ok(Some(existing_state)) = state::load_state(&canonical) {
                    let default_profile = crate::core::profile::load_profile(&canonical, "default");
                    for wt in &existing_state.worktrees {
                        if wt.phase.is_active() {
                            let profile_for_card = if wt.role.as_deref() == Some("reviewer") {
                                crate::core::profile::REVIEWER_PROFILE.to_string()
                            } else {
                                default_profile.clone()
                            };
                            workers.insert(
                                wt.id.clone(),
                                ManagedWorker {
                                    id: wt.id.clone(),
                                    branch: wt.branch.clone(),
                                    prompt: wt.prompt.clone(),
                                    kind: wt.agent_kind.clone(),
                                    repo_path: wt.repo_path.clone(),
                                    worktree_path: wt.worktree_path.clone(),
                                    phase: wt.phase.clone(),
                                    session_id: wt.session_id.clone(),
                                    restart_count: wt.restart_count.unwrap_or(0),
                                    pr: wt.pr.clone(),
                                    created_at: wt.created_at,
                                    message_tx: None,
                                    agent_card: build_agent_card(
                                        &wt.id,
                                        &git::repo_name(&wt.repo_path),
                                        wt.agent_kind.label(),
                                        &profile_for_card,
                                    ),
                                    role: wt.role.clone(),
                                    review_pr: wt.review_pr,
                                    accumulated_text: String::new(),
                                    review_verdict: wt.review_verdict.clone(),
                                    ready_branch: wt.ready_branch.clone(),
                                },
                            );
                        }
                    }
                }
                let worker_count = workers.len();
                let repo_count = repos.len();
                let inbox_pos = inbox_file_size(&canonical);
                let config = crate::core::config::load_config(&canonical);
                let ws = WorkspaceState {
                    path: canonical.clone(),
                    repos,
                    workers,
                    inbox_offset: inbox_pos,
                    close_on_pr_merge: config.close_on_pr_merge,
                };
                tracing::info!(
                    path = %canonical.display(),
                    repos = repo_count,
                    workers = worker_count,
                    "Registered workspace",
                );
                Some((canonical, ws))
            }))
        } else {
            None
        };

    // IDs queued for immediate PR polling (from TriggerPrPoll requests)
    let mut triggered_pr_poll_ids: Vec<String> = Vec::new();

    loop {
        // Check if deferred workspace registration completed
        if let Some(ref task) = pending_register
            && task.is_finished()
            && let Some(task) = pending_register.take()
            && let Ok(Some((path, ws))) = task.await
        {
            workspaces.insert(path.clone(), ws);

            // Re-spawn agent processes for active workers restored from state.json
            if let Some(ws) = workspaces.get_mut(&path) {
                let active_ids: Vec<String> = ws
                    .workers
                    .values()
                    .filter(|w| w.phase.is_active())
                    .map(|w| w.id.clone())
                    .collect();

                for id in active_ids {
                    let worker = ws.workers.get(&id).unwrap();
                    let msg_tx = spawn_worker_agent(
                        worker.id.clone(),
                        worker.kind.clone(),
                        worker.prompt.clone(),
                        worker.worktree_path.clone(),
                        ws.path.clone(),
                        worker.session_id.clone(),
                        worker.restart_count,
                        event_tx.clone(),
                        supervisor_tx.clone(),
                    );
                    ws.workers.get_mut(&id).unwrap().message_tx = Some(msg_tx);
                    tracing::info!(worker_id = %id, "Re-spawned agent");
                }
            }
        }

        // Check if background PR poll completed
        if let Some(ref task) = pending_pr_poll
            && task.is_finished()
            && let Some(task) = pending_pr_poll.take()
            && let Ok(results) = task.await
        {
            apply_pr_poll_results(results, &mut workspaces, &mut state_dirty, &event_tx);
        }

        tokio::select! {
            // Signal handlers
            _ = sigterm.recv() => {
                tracing::info!("Received SIGTERM, shutting down");
                break;
            }
            _ = sigint.recv() => {
                tracing::info!("Received SIGINT, shutting down");
                break;
            }

            // Socket requests from clients
            Some((request, resp_tx)) = request_rx.recv() => {
                handle_request(
                    request,
                    &resp_tx,
                    &mut workspaces,
                    &event_tx,
                    &supervisor_tx,
                    &mut state_dirty,
                    &mut triggered_pr_poll_ids,
                    a2a_port,
                ).await;
            }

            // HTTP requests from the A2A HTTP server
            Some((request, resp_tx)) = http_req_rx.recv() => {
                handle_request(
                    request,
                    &resp_tx,
                    &mut workspaces,
                    &event_tx,
                    &supervisor_tx,
                    &mut state_dirty,
                    &mut triggered_pr_poll_ids,
                    a2a_port,
                ).await;
            }

            // Supervisor events from agent tasks
            Some(event) = supervisor_rx.recv() => {
                match event {
                    SupervisorEvent::PhaseChanged { worktree_id, phase, session_id } => {
                        // Find the worker across all workspaces
                        for ws in workspaces.values_mut() {
                            if let Some(worker) = ws.workers.get_mut(&worktree_id) {
                                let old_phase = worker.phase.clone();
                                worker.phase = phase.clone();
                                if let Some(sid) = session_id {
                                    worker.session_id = Some(sid);
                                }
                                state_dirty = true;

                                // Emit phase change event to file
                                let _ = ipc::emit_event(&ws.path, &ipc::SwarmEvent::PhaseChanged {
                                    worktree: worktree_id.clone(),
                                    from: old_phase.clone(),
                                    to: phase.clone(),
                                    timestamp: Local::now(),
                                });

                                // Broadcast StateChanged (backward compat)
                                let _ = event_tx.send(DaemonResponse::StateChanged {
                                    worktree_id: worktree_id.clone(),
                                    phase: phase.clone(),
                                });

                                // Emit A2aTaskUpdate on meaningful transitions
                                let a2a_update = match &phase {
                                    WorkerPhase::Running
                                        if matches!(
                                            old_phase,
                                            WorkerPhase::Starting | WorkerPhase::Creating
                                        ) =>
                                    {
                                        Some((
                                            a2a_types::TaskState::Working,
                                            Some(A2aTaskMessage::text("Task started")),
                                        ))
                                    }
                                    WorkerPhase::Waiting => Some((
                                        a2a_types::TaskState::InputRequired,
                                        Some(A2aTaskMessage::text("Waiting for input")),
                                    )),
                                    WorkerPhase::Completed => Some((
                                        a2a_types::TaskState::Completed,
                                        Some(A2aTaskMessage::text("Task completed")),
                                    )),
                                    WorkerPhase::Failed => Some((
                                        a2a_types::TaskState::Failed,
                                        Some(A2aTaskMessage::text("Task failed")),
                                    )),
                                    _ => None,
                                };
                                if let Some((task_state, message)) = a2a_update {
                                    let _ = event_tx.send(DaemonResponse::A2aTaskUpdate {
                                        worktree_id: worktree_id.clone(),
                                        task_state,
                                        message,
                                        timestamp: chrono::Utc::now(),
                                    });
                                }
                                break;
                            }
                        }
                    }
                    SupervisorEvent::AgentEvent { worktree_id, event } => {
                        // Accumulate text output for reviewer workers so we can
                        // parse the review verdict when the agent finishes.
                        // Also scan all workers for BRANCH_READY signals.
                        if let protocol::AgentEventWire::TextDelta { ref text } = event {
                            for ws in workspaces.values_mut() {
                                if let Some(worker) = ws.workers.get_mut(&worktree_id) {
                                    worker.accumulated_text.push_str(text);
                                    if worker.role.as_deref() == Some("reviewer") {
                                        let had_verdict = worker.review_verdict.is_some();
                                        if let Some(verdict) =
                                            crate::core::state::parse_review_verdict(
                                                &worker.accumulated_text,
                                            )
                                        {
                                            // Emit A2aTaskUpdate on first parse
                                            if !had_verdict {
                                                let (task_state, approved) =
                                                    if verdict.approved {
                                                        (a2a_types::TaskState::Completed, true)
                                                    } else {
                                                        (
                                                            a2a_types::TaskState::InputRequired,
                                                            false,
                                                        )
                                                    };
                                                let comments: Vec<serde_json::Value> = verdict
                                                    .comments
                                                    .iter()
                                                    .map(|c| serde_json::Value::String(c.clone()))
                                                    .collect();
                                                let _ = event_tx.send(
                                                    DaemonResponse::A2aTaskUpdate {
                                                        worktree_id: worktree_id.clone(),
                                                        task_state,
                                                        message: Some(A2aTaskMessage::data(
                                                            "application/json",
                                                            serde_json::json!({
                                                                "type": "review_verdict",
                                                                "approved": approved,
                                                                "comments": comments,
                                                            }),
                                                        )),
                                                        timestamp: chrono::Utc::now(),
                                                    },
                                                );
                                            }
                                            worker.review_verdict = Some(verdict);
                                            state_dirty = true;
                                        }
                                    } else if worker.ready_branch.is_none()
                                        && let Some(branch) =
                                            crate::core::state::parse_branch_ready(
                                                &worker.accumulated_text,
                                            )
                                    {
                                        let _ = event_tx.send(DaemonResponse::A2aTaskUpdate {
                                            worktree_id: worktree_id.clone(),
                                            task_state: a2a_types::TaskState::Working,
                                            message: Some(A2aTaskMessage::data(
                                                "application/json",
                                                serde_json::json!({
                                                    "type": "branch_ready",
                                                    "branch": branch.clone(),
                                                }),
                                            )),
                                            timestamp: chrono::Utc::now(),
                                        });
                                        worker.ready_branch = Some(branch);
                                        state_dirty = true;
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            // Poll inbox.jsonl for each workspace
            _ = inbox_poll.tick() => {
                // Collect inbox messages with their workspace paths first,
                // then process them (avoids borrow conflict with handle_request).
                let inbox_batch: Vec<(PathBuf, Vec<ipc::InboxMessage>)> = workspaces
                    .values_mut()
                    .filter_map(|ws| {
                        if let Ok((messages, new_offset)) = ipc::read_inbox(&ws.path, ws.inbox_offset) {
                            ws.inbox_offset = new_offset;
                            if messages.is_empty() {
                                None
                            } else {
                                Some((ws.path.clone(), messages))
                            }
                        } else {
                            None
                        }
                    })
                    .collect();

                for (ws_path, messages) in inbox_batch {
                    for msg in messages {
                        let mut req = protocol::translate_inbox_message(&msg);
                        // Inject workspace into CreateWorker from inbox
                        if let DaemonRequest::CreateWorker { ref mut workspace, .. } = req
                            && workspace.is_none()
                        {
                            *workspace = Some(ws_path.clone());
                        }
                        let (resp_tx, _) = mpsc::unbounded_channel();
                        handle_request(
                            req,
                            &resp_tx,
                            &mut workspaces,
                            &event_tx,
                            &supervisor_tx,
                            &mut state_dirty,
                            &mut triggered_pr_poll_ids,
                            a2a_port,
                        ).await;
                    }
                }
            }

            // PR polling — spawn in background so we don't block the select loop.
            // Each `gh pr list` call can take seconds; with many workers this
            // would make the daemon unresponsive to socket requests.
            _ = pr_poll.tick() => {
                if pending_pr_poll.is_none() {
                    let jobs: Vec<PrPollJob> = workspaces.values()
                        .flat_map(|ws| {
                            ws.workers.values()
                                .filter(|w| w.phase.is_active() || (w.phase == WorkerPhase::Waiting && w.pr.is_none()))
                                .map(|w| PrPollJob {
                                    worker_id: w.id.clone(),
                                    branch: w.branch.clone(),
                                    repo_path: w.repo_path.clone(),
                                    worktree_path: w.worktree_path.clone(),
                                    had_pr: w.pr.is_some(),
                                    workspace_path: ws.path.clone(),
                                })
                        })
                        .collect();
                    if !jobs.is_empty() {
                        pending_pr_poll = Some(tokio::task::spawn_blocking(move || {
                            poll_prs_background(jobs)
                        }));
                    }
                }
            }
        }

        // Triggered PR poll: if IDs were queued and no poll is in flight, run one now
        if !triggered_pr_poll_ids.is_empty() && pending_pr_poll.is_none() {
            let ids = std::mem::take(&mut triggered_pr_poll_ids);
            let jobs: Vec<PrPollJob> = workspaces
                .values()
                .flat_map(|ws| {
                    ws.workers
                        .values()
                        .filter(|w| ids.contains(&w.id))
                        .map(|w| PrPollJob {
                            worker_id: w.id.clone(),
                            branch: w.branch.clone(),
                            repo_path: w.repo_path.clone(),
                            worktree_path: w.worktree_path.clone(),
                            had_pr: w.pr.is_some(),
                            workspace_path: ws.path.clone(),
                        })
                })
                .collect();
            if !jobs.is_empty() {
                pending_pr_poll = Some(tokio::task::spawn_blocking(move || {
                    poll_prs_background(jobs)
                }));
            }
        }

        // Debounced state save
        if state_dirty && last_state_save.elapsed() >= state_save_interval {
            save_all_workspace_states(&workspaces);
            state_dirty = false;
            last_state_save = std::time::Instant::now();
        }
    }

    // Graceful shutdown
    tracing::info!("Interrupting active agents");

    // Final state save
    save_all_workspace_states(&workspaces);
    remove_pid();
    tracing::info!("Daemon stopped");

    Ok(())
}

// ── Agent spawning ───────────────────────────────────────

/// Spawn (or re-spawn) a worker agent process and its event loop.
///
/// Returns the `mpsc::UnboundedSender<String>` used to send follow-up messages.
/// The agent task and its panic-watcher are spawned as background tokio tasks.
#[allow(clippy::too_many_arguments)]
fn spawn_worker_agent(
    worker_id: String,
    kind: AgentKind,
    prompt: String,
    worktree_path: PathBuf,
    work_dir: PathBuf,
    resume_session_id: Option<String>,
    initial_restart_count: u32,
    event_tx: broadcast::Sender<DaemonResponse>,
    supervisor_tx: mpsc::UnboundedSender<SupervisorEvent>,
) -> mpsc::UnboundedSender<String> {
    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<String>();

    let wt_id = worker_id;
    let panic_wt_id = wt_id.clone();
    let wt_path = worktree_path;
    let wd = work_dir;
    let ev_tx = event_tx;
    let sv_tx = supervisor_tx;

    let join_handle = tokio::spawn(async move {
        tracing::debug!(worker_id = %wt_id, "Agent spawning process");
        let handle_result = agent_supervisor::spawn_agent(agent_supervisor::SpawnAgentOpts {
            worktree_id: &wt_id,
            kind: kind.clone(),
            prompt: &prompt,
            worktree_path: &wt_path,
            work_dir: &wd,
            resume_session_id,
            dangerously_skip_permissions: true,
            event_tx: ev_tx.clone(),
        })
        .await;

        let mut handle = match handle_result {
            Ok(h) => h,
            Err(e) => {
                tracing::error!(worker_id = %wt_id, error = %e, "Failed to spawn agent");
                let _ = sv_tx.send(SupervisorEvent::PhaseChanged {
                    worktree_id: wt_id,
                    phase: WorkerPhase::Failed,
                    session_id: None,
                });
                return;
            }
        };

        handle.logger.log_start(&prompt, None);
        tracing::info!(worker_id = %wt_id, kind = ?kind, "Agent spawned successfully");

        let _ = sv_tx.send(SupervisorEvent::PhaseChanged {
            worktree_id: wt_id.clone(),
            phase: WorkerPhase::Running,
            session_id: None,
        });

        // Initial event loop
        tracing::debug!(worker_id = %wt_id, "Agent entering event loop");
        let mut restart_count = initial_restart_count;
        let (phase, _session_id) = agent_supervisor::agent_event_loop(
            &mut handle,
            agent_supervisor::EventLoopOpts {
                supervisor_tx: &sv_tx,
                work_dir: &wd,
                restart_count: &mut restart_count,
                kind: kind.clone(),
                prompt: &prompt,
                worktree_path: &wt_path,
                dangerously_skip_permissions: true,
            },
        )
        .await;

        tracing::debug!(worker_id = %wt_id, ?phase, "Agent event loop exited");

        // If the agent is waiting, listen for follow-up messages
        if phase == WorkerPhase::Waiting {
            loop {
                let message = match msg_rx.recv().await {
                    Some(msg) => msg,
                    None => break,
                };

                handle.logger.log_user_message(&message);
                tracing::debug!(worker_id = %wt_id, bytes = message.len(), "Sending follow-up message");

                if let Err(e) = handle.agent.send_message(&message).await {
                    tracing::error!(worker_id = %wt_id, error = %e, "Failed to send message");
                    let _ = sv_tx.send(SupervisorEvent::PhaseChanged {
                        worktree_id: wt_id.clone(),
                        phase: WorkerPhase::Failed,
                        session_id: handle.agent.session_id().map(String::from),
                    });
                    break;
                }

                let _ = sv_tx.send(SupervisorEvent::PhaseChanged {
                    worktree_id: wt_id.clone(),
                    phase: WorkerPhase::Running,
                    session_id: handle.agent.session_id().map(String::from),
                });

                let (new_phase, _) = agent_supervisor::agent_event_loop_with_timeouts(
                    &mut handle,
                    agent_supervisor::EventLoopOpts {
                        supervisor_tx: &sv_tx,
                        work_dir: &wd,
                        restart_count: &mut restart_count,
                        kind: kind.clone(),
                        prompt: &prompt,
                        worktree_path: &wt_path,
                        dangerously_skip_permissions: true,
                    },
                    None, // use default SEND_FIRST_EVENT_TIMEOUT
                    None, // use default SEND_TOTAL_TIMEOUT
                )
                .await;

                if new_phase != WorkerPhase::Waiting {
                    break;
                }
            }
        }
    });

    // Log if the agent task panics
    tokio::spawn(async move {
        if let Err(e) = join_handle.await {
            tracing::error!(worker_id = %panic_wt_id, error = ?e, "Agent task panicked");
        }
    });

    msg_tx
}

// ── Reviewer worktree setup ──────────────────────────────

/// Fetch the PR branch and check it out inside the reviewer's worktree.
///
/// After `git worktree add` creates the worktree on a temp branch, we switch
/// to the actual PR branch so the agent can review the real diff.
/// Errors are logged but not fatal — the reviewer can still run on the temp branch.
fn setup_reviewer_worktree(repo_path: &Path, worktree_path: &Path, pr_number: u64) {
    // Get the PR's head branch name via gh.
    let gh_output = std::process::Command::new("gh")
        .args([
            "pr",
            "view",
            &pr_number.to_string(),
            "--json",
            "headRefName",
            "-q",
            ".headRefName",
        ])
        .current_dir(repo_path)
        .output();

    let pr_branch = match gh_output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        Ok(out) => {
            tracing::warn!(
                pr = pr_number,
                stderr = %String::from_utf8_lossy(&out.stderr),
                "gh pr view failed — reviewer will run on temp branch"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                pr = pr_number,
                error = %e,
                "Failed to run gh pr view — reviewer will run on temp branch"
            );
            return;
        }
    };

    if pr_branch.is_empty() {
        tracing::warn!(
            pr = pr_number,
            "Got empty branch name from gh pr view — reviewer will run on temp branch"
        );
        return;
    }

    tracing::info!(pr = pr_number, branch = %pr_branch, "Setting up reviewer worktree for PR branch");

    // Fetch the specific branch from origin.
    let fetch_status = std::process::Command::new("git")
        .args(["fetch", "origin", &pr_branch])
        .current_dir(repo_path)
        .status();

    if let Err(e) = fetch_status {
        tracing::warn!(pr = pr_number, error = %e, "git fetch failed for PR branch");
    }

    // Checkout the PR branch inside the worktree, force-resetting if it exists locally.
    let checkout_status = std::process::Command::new("git")
        .args([
            "checkout",
            "-B",
            &pr_branch,
            &format!("origin/{}", pr_branch),
        ])
        .current_dir(worktree_path)
        .status();

    match checkout_status {
        Ok(s) if s.success() => {
            tracing::info!(
                pr = pr_number,
                branch = %pr_branch,
                "Checked out PR branch in reviewer worktree"
            );
        }
        Ok(s) => {
            tracing::warn!(
                pr = pr_number,
                branch = %pr_branch,
                exit_code = ?s.code(),
                "git checkout of PR branch failed — reviewer will run on temp branch"
            );
        }
        Err(e) => {
            tracing::warn!(
                pr = pr_number,
                error = %e,
                "Failed to run git checkout — reviewer will run on temp branch"
            );
        }
    }
}

// ── Request handling ─────────────────────────────────────

/// Handle a daemon request.
#[allow(clippy::too_many_arguments)]
async fn handle_request(
    request: DaemonRequest,
    resp_tx: &mpsc::UnboundedSender<DaemonResponse>,
    workspaces: &mut HashMap<PathBuf, WorkspaceState>,
    event_tx: &broadcast::Sender<DaemonResponse>,
    supervisor_tx: &mpsc::UnboundedSender<SupervisorEvent>,
    state_dirty: &mut bool,
    triggered_pr_poll_ids: &mut Vec<String>,
    a2a_port: u16,
) {
    match request {
        DaemonRequest::Ping => {
            let _ = resp_tx.send(DaemonResponse::Ok { data: None });
        }

        DaemonRequest::Auth { .. } => {
            let _ = resp_tx.send(DaemonResponse::Ok { data: None });
        }

        DaemonRequest::RegisterWorkspace { path } => {
            match register_workspace(workspaces, &path).await {
                Ok(canonical) => {
                    let _ = resp_tx.send(DaemonResponse::Ok {
                        data: Some(serde_json::json!({ "path": canonical.to_string_lossy() })),
                    });
                }
                Err(e) => {
                    let _ = resp_tx.send(DaemonResponse::Error {
                        message: format!("failed to register workspace: {}", e),
                    });
                }
            }
        }

        DaemonRequest::UnregisterWorkspace { path } => {
            let canonical = std::fs::canonicalize(&path).unwrap_or(path);
            if workspaces.remove(&canonical).is_some() {
                tracing::info!(path = %canonical.display(), "Unregistered workspace");
                let _ = resp_tx.send(DaemonResponse::Ok { data: None });
            } else {
                let _ = resp_tx.send(DaemonResponse::Error {
                    message: format!("workspace not registered: {}", canonical.display()),
                });
            }
        }

        DaemonRequest::ListWorkspaces => {
            let infos: Vec<WorkspaceInfo> = workspaces
                .values()
                .map(|ws| WorkspaceInfo {
                    path: ws.path.clone(),
                    worker_count: ws.workers.len(),
                })
                .collect();
            let _ = resp_tx.send(DaemonResponse::Workspaces { workspaces: infos });
        }

        DaemonRequest::ListWorkers { workspace } => {
            let infos: Vec<WorkerInfo> = if let Some(ws_path) = workspace {
                let canonical = std::fs::canonicalize(&ws_path).unwrap_or(ws_path);
                workspaces
                    .get(&canonical)
                    .map(|ws| ws.workers.values().map(|w| w.to_worker_info()).collect())
                    .unwrap_or_default()
            } else {
                // All workers across all workspaces
                workspaces
                    .values()
                    .flat_map(|ws| ws.workers.values().map(|w| w.to_worker_info()))
                    .collect()
            };
            let _ = resp_tx.send(DaemonResponse::Workers { workers: infos });
        }

        DaemonRequest::CreateWorker {
            prompt,
            agent,
            repo,
            start_point,
            workspace,
            profile,
            task_dir,
            role,
            review_pr,
            base_branch,
        } => {
            // Check prerequisites (claude CLI required, gh optional)
            if let Err(msg) = crate::core::prerequisites::check_prerequisites() {
                let _ = resp_tx.send(DaemonResponse::Error { message: msg });
                return;
            }

            let kind = match agent.parse::<AgentKind>() {
                Ok(k) => k,
                Err(e) => {
                    let _ = resp_tx.send(DaemonResponse::Error {
                        message: e.to_string(),
                    });
                    return;
                }
            };

            // Resolve workspace
            let ws_path = match resolve_workspace(workspaces, workspace.as_deref()) {
                Ok(p) => p,
                Err(e) => {
                    let _ = resp_tx.send(DaemonResponse::Error {
                        message: e.to_string(),
                    });
                    return;
                }
            };

            let ws = match workspaces.get_mut(&ws_path) {
                Some(ws) => ws,
                None => {
                    let _ = resp_tx.send(DaemonResponse::Error {
                        message: format!("workspace not found: {}", ws_path.display()),
                    });
                    return;
                }
            };

            // Resolve repo path
            let repo_path = match resolve_repo(&ws.repos, repo.as_deref()) {
                Ok(p) => p,
                Err(e) => {
                    let _ = resp_tx.send(DaemonResponse::Error {
                        message: e.to_string(),
                    });
                    return;
                }
            };

            // Fetch latest from origin so worktrees start from up-to-date refs
            if let Err(e) = git::fetch_origin(&repo_path) {
                tracing::warn!(error = %e, "git fetch origin failed");
            }

            // Fast-forward local main so cargo builds use fresh code
            git::pull_main(&repo_path);

            // Default to origin/main when no explicit start_point
            let start_point = start_point.or_else(|| Some("origin/main".to_string()));

            // Generate IDs (short UUID suffix, e.g. "hive-a1b2")
            let short_id = &uuid::Uuid::new_v4().to_string()[..4];
            let repo_name = git::repo_name(&repo_path);
            let worktree_id = format!("{}-{}", repo_name, short_id);
            let branch = git::generate_branch_name_ai(&prompt, short_id).await;

            let work_dir = ws.path.clone();
            let worktree_path = work_dir.join(".swarm").join("wt").join(&worktree_id);

            // Create git worktree
            if let Err(e) =
                git::create_worktree(&repo_path, &branch, &worktree_path, start_point.as_deref())
            {
                let _ = resp_tx.send(DaemonResponse::Error {
                    message: format!("failed to create worktree: {}", e),
                });
                let _ = ipc::emit_event(
                    &work_dir,
                    &ipc::SwarmEvent::CreateFailed {
                        error: e.to_string(),
                        prompt: prompt.clone(),
                        repo,
                        timestamp: Local::now(),
                    },
                );
                return;
            }

            // Symlink .env* and other gitignored config files
            let linked = git::symlink_worktree_files(&repo_path, &worktree_path);
            if !linked.is_empty() {
                tracing::info!(
                    count = linked.len(),
                    files = %linked.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "),
                    "Symlinked config files into worktree"
                );
            }

            let is_reviewer = role.as_deref() == Some("reviewer");

            // For reviewer workers: fetch and checkout the PR branch in the worktree.
            if is_reviewer && let Some(pr_num) = review_pr {
                setup_reviewer_worktree(&repo_path, &worktree_path, pr_num);
            }

            // Load profile and build an effective prompt (profile + user prompt) for
            // the agent. The raw `prompt` is kept for display/persistence in WorkerState;
            // only the agent spawn call receives the effective prompt.
            let profile_content = if is_reviewer {
                crate::core::profile::REVIEWER_PROFILE.to_string()
            } else {
                let profile_slug = profile.as_deref().unwrap_or("default");
                crate::core::profile::load_profile(&work_dir, profile_slug)
            };

            let effective_prompt = if is_reviewer {
                let base = base_branch.as_deref().unwrap_or("main");
                let reviewer_task = match review_pr {
                    Some(n) => format!(
                        "Review the diff for PR #{}. Run `git diff {}...HEAD` to see changes.",
                        n, base
                    ),
                    None => format!(
                        "Review the diff for this PR. Run `git diff {}...HEAD` to see changes.",
                        base
                    ),
                };
                crate::core::profile::build_effective_prompt(&profile_content, &reviewer_task)
            } else {
                crate::core::profile::build_effective_prompt(&profile_content, &prompt)
            };

            // Seed .task/ directory if artifacts provided
            if let Some(ref payload) = task_dir {
                let task_path = worktree_path.join(".task");
                let _ = std::fs::create_dir_all(&task_path);
                if let Some(ref c) = payload.task_md {
                    let _ = std::fs::write(task_path.join("TASK.md"), c);
                }
                if let Some(ref c) = payload.context_md {
                    let _ = std::fs::write(task_path.join("CONTEXT.md"), c);
                }
                if let Some(ref c) = payload.plan_md {
                    let _ = std::fs::write(task_path.join("PLAN.md"), c);
                }
            }

            // Register the worker
            let worker = ManagedWorker {
                id: worktree_id.clone(),
                branch: branch.clone(),
                prompt: prompt.clone(),
                kind: kind.clone(),
                repo_path: repo_path.clone(),
                worktree_path: worktree_path.clone(),
                phase: WorkerPhase::Starting,
                session_id: None,
                restart_count: 0,
                pr: None,
                created_at: Local::now(),
                message_tx: None,
                agent_card: {
                    let mut card =
                        build_agent_card(&worktree_id, &repo_name, kind.label(), &profile_content);
                    if a2a_port != 0 {
                        let encoded_id =
                            crate::core::agent_card::url_encode_worker_id(&worktree_id);
                        card.url = format!("http://localhost:{a2a_port}/a2a/workers/{encoded_id}");
                    }
                    card
                },
                role: role.clone(),
                review_pr,
                accumulated_text: String::new(),
                review_verdict: None,
                ready_branch: None,
            };

            ws.workers.insert(worktree_id.clone(), worker);
            *state_dirty = true;

            // Spawn the agent in a background task
            let msg_tx = spawn_worker_agent(
                worktree_id.clone(),
                kind.clone(),
                effective_prompt,
                worktree_path.clone(),
                work_dir.clone(),
                None, // new worker, no session to resume
                0,
                event_tx.clone(),
                supervisor_tx.clone(),
            );
            if let Some(w) = ws.workers.get_mut(&worktree_id) {
                w.message_tx = Some(msg_tx);
            }

            // Emit creation event
            let _ = ipc::emit_event(
                &work_dir,
                &ipc::SwarmEvent::WorktreeCreated {
                    worktree: worktree_id.clone(),
                    branch,
                    agent: kind.label().to_string(),
                    pane_id: "daemon".to_string(),
                    timestamp: Local::now(),
                },
            );

            let _ = resp_tx.send(DaemonResponse::Ok {
                data: Some(serde_json::json!({ "worktree_id": worktree_id })),
            });
        }

        DaemonRequest::SendMessage {
            worktree_id,
            message,
        } => {
            // Scan all workspaces for the worker
            let found = find_worker_workspace_mut(workspaces, &worktree_id);
            if let Some((ws_path, worker)) = found {
                if let Some(ref msg_tx) = worker.message_tx {
                    if msg_tx.send(message.clone()).is_ok() {
                        let _ = ipc::write_agent_inbox(&ws_path, &worktree_id, &message);
                        let _ = resp_tx.send(DaemonResponse::Ok { data: None });
                    } else {
                        let _ = resp_tx.send(DaemonResponse::Error {
                            message: "agent task has ended".into(),
                        });
                    }
                } else {
                    let _ = resp_tx.send(DaemonResponse::Error {
                        message: "agent not accepting messages".into(),
                    });
                }
            } else {
                let _ = resp_tx.send(DaemonResponse::Error {
                    message: format!("unknown worker: {}", worktree_id),
                });
            }
        }

        DaemonRequest::CloseWorker { worktree_id } => {
            // Find which workspace owns this worker
            let ws_path = workspaces.values().find_map(|ws| {
                if ws.workers.contains_key(&worktree_id) {
                    Some(ws.path.clone())
                } else {
                    None
                }
            });

            if let Some(ws_path) = ws_path {
                if let Some(ws) = workspaces.get_mut(&ws_path) {
                    if let Some(worker) = ws.workers.get_mut(&worktree_id) {
                        worker.message_tx = None;
                        worker.phase = WorkerPhase::Completed;
                        *state_dirty = true;

                        let _ = git::remove_worktree(&worker.repo_path, &worker.worktree_path);
                        if git::checkout_main(&worker.repo_path).is_ok() {
                            git::pull_main(&worker.repo_path);
                        }
                        let _ = git::delete_branch(&worker.repo_path, &worker.branch);

                        let _ = ipc::emit_event(
                            &ws_path,
                            &ipc::SwarmEvent::WorktreeClosed {
                                worktree: worktree_id.clone(),
                                timestamp: Local::now(),
                            },
                        );

                        ws.workers.remove(&worktree_id);
                    }
                    let _ = resp_tx.send(DaemonResponse::Ok { data: None });
                } else {
                    let _ = resp_tx.send(DaemonResponse::Error {
                        message: format!("unknown worker: {}", worktree_id),
                    });
                }
            } else {
                let _ = resp_tx.send(DaemonResponse::Error {
                    message: format!("unknown worker: {}", worktree_id),
                });
            }
        }

        DaemonRequest::MergeWorker { worktree_id } => {
            let ws_path = workspaces.values().find_map(|ws| {
                if ws.workers.contains_key(&worktree_id) {
                    Some(ws.path.clone())
                } else {
                    None
                }
            });

            if let Some(ws_path) = ws_path {
                if let Some(ws) = workspaces.get_mut(&ws_path) {
                    if let Some(worker) = ws.workers.get_mut(&worktree_id) {
                        match crate::core::merge::commit_all_and_merge(
                            &worker.repo_path,
                            &worker.worktree_path,
                            &worker.branch,
                        ) {
                            Ok(()) => {
                                worker.phase = WorkerPhase::Completed;
                                *state_dirty = true;

                                let _ = ipc::emit_event(
                                    &ws_path,
                                    &ipc::SwarmEvent::WorktreeMerged {
                                        worktree: worktree_id.clone(),
                                        branch: worker.branch.clone(),
                                        timestamp: Local::now(),
                                    },
                                );

                                let _ =
                                    git::remove_worktree(&worker.repo_path, &worker.worktree_path);
                                if git::checkout_main(&worker.repo_path).is_ok() {
                                    git::pull_main(&worker.repo_path);
                                }
                                let _ = git::delete_branch(&worker.repo_path, &worker.branch);
                                ws.workers.remove(&worktree_id);

                                let _ = resp_tx.send(DaemonResponse::Ok { data: None });
                            }
                            Err(e) => {
                                let _ = resp_tx.send(DaemonResponse::Error {
                                    message: format!("merge failed: {}", e),
                                });
                            }
                        }
                    } else {
                        let _ = resp_tx.send(DaemonResponse::Error {
                            message: format!("unknown worker: {}", worktree_id),
                        });
                    }
                } else {
                    let _ = resp_tx.send(DaemonResponse::Error {
                        message: format!("unknown worker: {}", worktree_id),
                    });
                }
            } else {
                let _ = resp_tx.send(DaemonResponse::Error {
                    message: format!("unknown worker: {}", worktree_id),
                });
            }
        }

        DaemonRequest::Subscribe { .. } => {
            let _ = resp_tx.send(DaemonResponse::Ok { data: None });
        }

        DaemonRequest::TriggerPrPoll { worker_ids } => {
            triggered_pr_poll_ids.extend(worker_ids);
            let _ = resp_tx.send(DaemonResponse::Ok { data: None });
        }

        DaemonRequest::GetHistory { worktree_id } => {
            // Search all workspaces for the agent events file
            let mut found = false;
            for ws in workspaces.values() {
                let events_path = ws
                    .path
                    .join(".swarm")
                    .join("agents")
                    .join(&worktree_id)
                    .join("events.jsonl");
                if events_path.exists() {
                    match std::fs::read_to_string(&events_path) {
                        Ok(content) => {
                            let _ = resp_tx.send(DaemonResponse::Ok {
                                data: Some(serde_json::json!({ "events": content })),
                            });
                        }
                        Err(e) => {
                            let _ = resp_tx.send(DaemonResponse::Error {
                                message: format!("failed to read events: {}", e),
                            });
                        }
                    }
                    found = true;
                    break;
                }
            }
            if !found {
                let _ = resp_tx.send(DaemonResponse::Ok {
                    data: Some(serde_json::json!({ "events": "" })),
                });
            }
        }
    }
}

// ── Helpers ──────────────────────────────────────────────

/// Resolve a workspace path from the registered workspaces.
fn resolve_workspace(
    workspaces: &HashMap<PathBuf, WorkspaceState>,
    workspace: Option<&Path>,
) -> Result<PathBuf> {
    match workspace {
        Some(path) => {
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            if workspaces.contains_key(&canonical) {
                Ok(canonical)
            } else {
                Err(color_eyre::eyre::eyre!(
                    "workspace not registered: {} (register it first)",
                    canonical.display()
                ))
            }
        }
        None => {
            if workspaces.len() == 1 {
                Ok(workspaces.keys().next().unwrap().clone())
            } else if workspaces.is_empty() {
                Err(color_eyre::eyre::eyre!("no workspaces registered"))
            } else {
                let paths: Vec<String> =
                    workspaces.keys().map(|p| p.display().to_string()).collect();
                Err(color_eyre::eyre::eyre!(
                    "multiple workspaces registered, specify workspace: {}",
                    paths.join(", ")
                ))
            }
        }
    }
}

/// Resolve a repo path from the detected repos list.
fn resolve_repo(repos: &[PathBuf], repo_name: Option<&str>) -> Result<PathBuf> {
    match repo_name {
        Some(name) => repos
            .iter()
            .find(|r| git::repo_name(r) == name)
            .cloned()
            .ok_or_else(|| {
                let available: Vec<String> = repos.iter().map(|r| git::repo_name(r)).collect();
                color_eyre::eyre::eyre!(
                    "unknown repo '{}' (available: {})",
                    name,
                    available.join(", ")
                )
            }),
        None => {
            if repos.len() == 1 {
                Ok(repos[0].clone())
            } else if repos.is_empty() {
                Err(color_eyre::eyre::eyre!("no git repos detected"))
            } else {
                let names: Vec<String> = repos.iter().map(|r| git::repo_name(r)).collect();
                Err(color_eyre::eyre::eyre!(
                    "multiple repos detected, specify --repo: {}",
                    names.join(", ")
                ))
            }
        }
    }
}

/// Find a worker by ID across all workspaces. Returns (workspace_path, &mut worker).
fn find_worker_workspace_mut<'a>(
    workspaces: &'a mut HashMap<PathBuf, WorkspaceState>,
    worktree_id: &str,
) -> Option<(PathBuf, &'a mut ManagedWorker)> {
    for ws in workspaces.values_mut() {
        if let Some(worker) = ws.workers.get_mut(worktree_id) {
            return Some((ws.path.clone(), worker));
        }
    }
    None
}

/// Save state.json for each registered workspace.
fn save_all_workspace_states(workspaces: &HashMap<PathBuf, WorkspaceState>) {
    for ws in workspaces.values() {
        save_daemon_state(&ws.path, &ws.workers);
    }
}

/// Save the daemon's worker state to state.json for a single workspace.
fn save_daemon_state(work_dir: &Path, workers: &HashMap<String, ManagedWorker>) {
    let worktrees: Vec<WorktreeState> = workers.values().map(|w| w.to_worktree_state()).collect();

    let dir_name = work_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "swarm".to_string());

    let state = SwarmState {
        session_name: format!("swarm-{}", dir_name),
        sidebar_pane_id: None,
        worktrees,
        last_inbox_pos: 0, // daemon manages its own inbox offset
    };

    if let Err(e) = state::save_state(work_dir, &state) {
        tracing::error!(path = %work_dir.display(), error = %e, "Failed to save state");
    }
}

// ── Background PR polling ────────────────────────────────

/// Input for a single PR poll job (sent to the background thread).
struct PrPollJob {
    worker_id: String,
    branch: String,
    repo_path: PathBuf,
    worktree_path: PathBuf,
    /// Whether this worker already had a PR before this poll.
    had_pr: bool,
    workspace_path: PathBuf,
}

/// Result of a single PR poll (returned from the background thread).
struct PrPollResult {
    worker_id: String,
    workspace_path: PathBuf,
    pr: PrInfo,
    is_new: bool,
}

/// Try `gh pr view` from the worktree directory (uses git context to find the PR).
/// Returns `None` when no PR exists (gh exits non-zero).
fn try_gh_pr_view(worktree_path: &Path) -> Option<PrInfo> {
    let output = std::process::Command::new("gh")
        .args(["pr", "view", "--json", "number,title,state,url"])
        .current_dir(worktree_path)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let pr: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    Some(PrInfo {
        number: pr["number"].as_u64().unwrap_or(0),
        title: pr["title"].as_str().unwrap_or("").to_string(),
        state: pr["state"].as_str().unwrap_or("").to_string(),
        url: pr["url"].as_str().unwrap_or("").to_string(),
    })
}

/// Fallback: `gh pr list --head <branch>` from the repo directory.
fn try_gh_pr_list(repo_path: &Path, branch: &str) -> Option<PrInfo> {
    let output = std::process::Command::new("gh")
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
        .current_dir(repo_path)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let prs: Vec<serde_json::Value> = serde_json::from_str(text.trim()).ok()?;
    let pr = prs.first()?;
    Some(PrInfo {
        number: pr["number"].as_u64().unwrap_or(0),
        title: pr["title"].as_str().unwrap_or("").to_string(),
        state: pr["state"].as_str().unwrap_or("").to_string(),
        url: pr["url"].as_str().unwrap_or("").to_string(),
    })
}

/// Run PR polling on a blocking thread. Tries `gh pr view` first (uses git
/// context from the worktree), then falls back to `gh pr list --head <branch>`.
fn poll_prs_background(jobs: Vec<PrPollJob>) -> Vec<PrPollResult> {
    let mut results = Vec::new();
    for job in &jobs {
        let pr = try_gh_pr_view(&job.worktree_path)
            .or_else(|| try_gh_pr_list(&job.repo_path, &job.branch));

        if let Some(new_pr) = pr {
            results.push(PrPollResult {
                worker_id: job.worker_id.clone(),
                workspace_path: job.workspace_path.clone(),
                pr: new_pr,
                is_new: !job.had_pr,
            });
        }
    }
    results
}

/// Apply PR poll results back to workspace state (runs on the main event loop).
fn apply_pr_poll_results(
    results: Vec<PrPollResult>,
    workspaces: &mut HashMap<PathBuf, WorkspaceState>,
    state_dirty: &mut bool,
    event_tx: &broadcast::Sender<DaemonResponse>,
) {
    // Collect worker IDs to remove after the loop (can't remove while iterating).
    let mut to_remove: Vec<(PathBuf, String)> = Vec::new();

    for result in results {
        if let Some(ws) = workspaces.get_mut(&result.workspace_path)
            && let Some(worker) = ws.workers.get_mut(&result.worker_id)
        {
            if result.is_new {
                tracing::info!(
                    worker_id = %worker.id,
                    pr_number = result.pr.number,
                    pr_title = %result.pr.title,
                    "PR detected",
                );
                let _ = ipc::emit_event(
                    &result.workspace_path,
                    &ipc::SwarmEvent::PrDetected {
                        worktree: worker.id.clone(),
                        pr_url: result.pr.url.clone(),
                        pr_title: result.pr.title.clone(),
                        pr_number: result.pr.number,
                        timestamp: Local::now(),
                    },
                );
                let _ = event_tx.send(DaemonResponse::A2aTaskUpdate {
                    worktree_id: worker.id.clone(),
                    task_state: a2a_types::TaskState::Working,
                    message: Some(A2aTaskMessage::data(
                        "application/json",
                        serde_json::json!({
                            "type": "pr_opened",
                            "pr_url": result.pr.url.clone(),
                            "pr_number": result.pr.number,
                        }),
                    )),
                    timestamp: chrono::Utc::now(),
                });
            }

            let is_merged = result.pr.state == "MERGED";
            worker.pr = Some(result.pr);
            *state_dirty = true;

            // Auto-close workers whose PR has been merged (if enabled for this workspace)
            if is_merged && ws.close_on_pr_merge {
                tracing::info!(
                    worker_id = %worker.id,
                    pr_number = worker.pr.as_ref().unwrap().number,
                    "Auto-closing worker, PR merged",
                );
                worker.message_tx = None;
                worker.phase = WorkerPhase::Completed;
                let _ = git::remove_worktree(&worker.repo_path, &worker.worktree_path);
                if git::checkout_main(&worker.repo_path).is_ok() {
                    git::pull_main(&worker.repo_path);
                }
                let _ = git::delete_branch(&worker.repo_path, &worker.branch);
                let _ = ipc::emit_event(
                    &result.workspace_path,
                    &ipc::SwarmEvent::WorktreeClosed {
                        worktree: worker.id.clone(),
                        timestamp: Local::now(),
                    },
                );
                let _ = event_tx.send(DaemonResponse::StateChanged {
                    worktree_id: worker.id.clone(),
                    phase: WorkerPhase::Completed,
                });
                let _ = event_tx.send(DaemonResponse::A2aTaskUpdate {
                    worktree_id: worker.id.clone(),
                    task_state: a2a_types::TaskState::Completed,
                    message: Some(A2aTaskMessage::text("Task completed")),
                    timestamp: chrono::Utc::now(),
                });
                to_remove.push((result.workspace_path.clone(), worker.id.clone()));
            }
        }
    }

    // Remove auto-closed workers from their workspaces
    for (ws_path, worker_id) in to_remove {
        if let Some(ws) = workspaces.get_mut(&ws_path) {
            ws.workers.remove(&worker_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a ManagedWorker with sensible defaults for testing.
    fn test_worker(id: &str) -> ManagedWorker {
        ManagedWorker {
            id: id.to_string(),
            branch: format!("swarm/{}", id),
            prompt: "test prompt".to_string(),
            kind: AgentKind::Claude,
            repo_path: PathBuf::from("/tmp/repo"),
            worktree_path: PathBuf::from(format!("/tmp/wt/{}", id)),
            phase: WorkerPhase::Running,
            session_id: None,
            restart_count: 0,
            pr: None,
            created_at: Local::now(),
            message_tx: None,
            agent_card: build_agent_card(id, "repo", "claude", "## Testing\nRun tests."),
            role: None,
            review_pr: None,
            accumulated_text: String::new(),
            review_verdict: None,
            ready_branch: None,
        }
    }

    /// Build channels needed for handle_request tests.
    fn test_harness() -> (
        mpsc::UnboundedReceiver<DaemonResponse>,
        mpsc::UnboundedSender<DaemonResponse>,
        HashMap<PathBuf, WorkspaceState>,
        broadcast::Sender<DaemonResponse>,
        mpsc::UnboundedSender<SupervisorEvent>,
    ) {
        let (resp_tx, resp_rx) = mpsc::unbounded_channel();
        let workspaces = HashMap::new();
        let (event_tx, _) = broadcast::channel(16);
        let (supervisor_tx, _) = mpsc::unbounded_channel();
        (resp_rx, resp_tx, workspaces, event_tx, supervisor_tx)
    }

    /// Create a test workspace with workers.
    fn test_workspace(path: &str, worker_ids: Vec<&str>) -> WorkspaceState {
        let mut ws_workers = HashMap::new();
        for id in worker_ids {
            ws_workers.insert(id.to_string(), test_worker(id));
        }
        WorkspaceState {
            path: PathBuf::from(path),
            repos: vec![],
            workers: ws_workers,
            inbox_offset: 0,
            close_on_pr_merge: true,
        }
    }

    #[tokio::test]
    async fn handle_request_ping() {
        let (mut resp_rx, resp_tx, mut workspaces, event_tx, supervisor_tx) = test_harness();
        let mut state_dirty = false;
        let mut triggered = Vec::new();

        handle_request(
            DaemonRequest::Ping,
            &resp_tx,
            &mut workspaces,
            &event_tx,
            &supervisor_tx,
            &mut state_dirty,
            &mut triggered,
            0,
        )
        .await;

        let resp = resp_rx.try_recv().unwrap();
        assert!(matches!(resp, DaemonResponse::Ok { data: None }));
    }

    #[tokio::test]
    async fn handle_request_list_workers_empty() {
        let (mut resp_rx, resp_tx, mut workspaces, event_tx, supervisor_tx) = test_harness();
        let mut state_dirty = false;
        let mut triggered = Vec::new();

        handle_request(
            DaemonRequest::ListWorkers { workspace: None },
            &resp_tx,
            &mut workspaces,
            &event_tx,
            &supervisor_tx,
            &mut state_dirty,
            &mut triggered,
            0,
        )
        .await;

        let resp = resp_rx.try_recv().unwrap();
        match resp {
            DaemonResponse::Workers { workers } => assert!(workers.is_empty()),
            other => panic!("expected Workers, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn handle_request_list_workers_filtered_by_workspace() {
        let (mut resp_rx, resp_tx, mut workspaces, event_tx, supervisor_tx) = test_harness();
        let mut state_dirty = false;
        let mut triggered = Vec::new();

        workspaces.insert(
            PathBuf::from("/tmp/ws1"),
            test_workspace("/tmp/ws1", vec!["hive-1"]),
        );
        workspaces.insert(
            PathBuf::from("/tmp/ws2"),
            test_workspace("/tmp/ws2", vec!["hive-2", "hive-3"]),
        );

        // List all
        handle_request(
            DaemonRequest::ListWorkers { workspace: None },
            &resp_tx,
            &mut workspaces,
            &event_tx,
            &supervisor_tx,
            &mut state_dirty,
            &mut triggered,
            0,
        )
        .await;

        let resp = resp_rx.try_recv().unwrap();
        match resp {
            DaemonResponse::Workers { workers } => assert_eq!(workers.len(), 3),
            other => panic!("expected Workers, got {:?}", other),
        }

        // List filtered
        handle_request(
            DaemonRequest::ListWorkers {
                workspace: Some(PathBuf::from("/tmp/ws2")),
            },
            &resp_tx,
            &mut workspaces,
            &event_tx,
            &supervisor_tx,
            &mut state_dirty,
            &mut triggered,
            0,
        )
        .await;

        let resp = resp_rx.try_recv().unwrap();
        match resp {
            DaemonResponse::Workers { workers } => assert_eq!(workers.len(), 2),
            other => panic!("expected Workers, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn handle_request_send_message_unknown_worker() {
        let (mut resp_rx, resp_tx, mut workspaces, event_tx, supervisor_tx) = test_harness();
        let mut state_dirty = false;
        let mut triggered = Vec::new();

        handle_request(
            DaemonRequest::SendMessage {
                worktree_id: "nonexistent".into(),
                message: "hello".into(),
            },
            &resp_tx,
            &mut workspaces,
            &event_tx,
            &supervisor_tx,
            &mut state_dirty,
            &mut triggered,
            0,
        )
        .await;

        let resp = resp_rx.try_recv().unwrap();
        match resp {
            DaemonResponse::Error { message } => {
                assert!(message.contains("unknown worker"));
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn handle_request_close_unknown_worker() {
        let (mut resp_rx, resp_tx, mut workspaces, event_tx, supervisor_tx) = test_harness();
        let mut state_dirty = false;
        let mut triggered = Vec::new();

        handle_request(
            DaemonRequest::CloseWorker {
                worktree_id: "nonexistent".into(),
            },
            &resp_tx,
            &mut workspaces,
            &event_tx,
            &supervisor_tx,
            &mut state_dirty,
            &mut triggered,
            0,
        )
        .await;

        let resp = resp_rx.try_recv().unwrap();
        match resp {
            DaemonResponse::Error { message } => {
                assert!(message.contains("unknown worker"));
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn handle_request_register_workspace() {
        let (mut resp_rx, resp_tx, mut workspaces, event_tx, supervisor_tx) = test_harness();
        let mut state_dirty = false;
        let mut triggered = Vec::new();

        let dir = tempfile::tempdir().unwrap();

        handle_request(
            DaemonRequest::RegisterWorkspace {
                path: dir.path().to_path_buf(),
            },
            &resp_tx,
            &mut workspaces,
            &event_tx,
            &supervisor_tx,
            &mut state_dirty,
            &mut triggered,
            0,
        )
        .await;

        let resp = resp_rx.try_recv().unwrap();
        assert!(matches!(resp, DaemonResponse::Ok { .. }));
        assert_eq!(workspaces.len(), 1);
    }

    #[tokio::test]
    async fn handle_request_list_workspaces() {
        let (mut resp_rx, resp_tx, mut workspaces, event_tx, supervisor_tx) = test_harness();
        let mut state_dirty = false;
        let mut triggered = Vec::new();

        workspaces.insert(
            PathBuf::from("/tmp/ws1"),
            test_workspace("/tmp/ws1", vec!["hive-1"]),
        );
        workspaces.insert(
            PathBuf::from("/tmp/ws2"),
            test_workspace("/tmp/ws2", vec![]),
        );

        handle_request(
            DaemonRequest::ListWorkspaces,
            &resp_tx,
            &mut workspaces,
            &event_tx,
            &supervisor_tx,
            &mut state_dirty,
            &mut triggered,
            0,
        )
        .await;

        let resp = resp_rx.try_recv().unwrap();
        match resp {
            DaemonResponse::Workspaces { workspaces } => {
                assert_eq!(workspaces.len(), 2);
            }
            other => panic!("expected Workspaces, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn handle_request_cross_workspace_worker_lookup() {
        let (mut resp_rx, resp_tx, mut workspaces, event_tx, supervisor_tx) = test_harness();
        let mut state_dirty = false;
        let mut triggered = Vec::new();

        workspaces.insert(
            PathBuf::from("/tmp/ws1"),
            test_workspace("/tmp/ws1", vec!["hive-1"]),
        );
        workspaces.insert(
            PathBuf::from("/tmp/ws2"),
            test_workspace("/tmp/ws2", vec!["hive-2"]),
        );

        // SendMessage to worker in ws2 without specifying workspace
        handle_request(
            DaemonRequest::SendMessage {
                worktree_id: "hive-2".into(),
                message: "hello".into(),
            },
            &resp_tx,
            &mut workspaces,
            &event_tx,
            &supervisor_tx,
            &mut state_dirty,
            &mut triggered,
            0,
        )
        .await;

        let resp = resp_rx.try_recv().unwrap();
        // Worker exists but has no message_tx (test worker), so we get "agent not accepting messages"
        match resp {
            DaemonResponse::Error { message } => {
                assert!(message.contains("agent not accepting messages"));
            }
            other => panic!("expected Error about agent, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn handle_request_trigger_pr_poll() {
        let (mut resp_rx, resp_tx, mut workspaces, event_tx, supervisor_tx) = test_harness();
        let mut state_dirty = false;
        let mut triggered = Vec::new();

        handle_request(
            DaemonRequest::TriggerPrPoll {
                worker_ids: vec!["w-1".into(), "w-2".into()],
            },
            &resp_tx,
            &mut workspaces,
            &event_tx,
            &supervisor_tx,
            &mut state_dirty,
            &mut triggered,
            0,
        )
        .await;

        let resp = resp_rx.try_recv().unwrap();
        assert!(matches!(resp, DaemonResponse::Ok { data: None }));
        assert_eq!(triggered, vec!["w-1", "w-2"]);
    }

    // ── Background PR poll tests ─────────────────────────

    #[test]
    fn apply_pr_poll_results_updates_worker() {
        let mut workspaces = HashMap::new();
        let ws_path = PathBuf::from("/tmp/ws");
        let ws = test_workspace("/tmp/ws", vec!["w-1"]);
        assert!(ws.workers.get("w-1").unwrap().pr.is_none());
        workspaces.insert(ws_path.clone(), ws);

        let results = vec![PrPollResult {
            worker_id: "w-1".to_string(),
            workspace_path: ws_path.clone(),
            pr: PrInfo {
                number: 42,
                title: "test pr".to_string(),
                state: "OPEN".to_string(),
                url: "https://github.com/test/repo/pull/42".to_string(),
            },
            is_new: true,
        }];

        let mut state_dirty = false;
        let (event_tx, _) = broadcast::channel(16);
        apply_pr_poll_results(results, &mut workspaces, &mut state_dirty, &event_tx);

        let worker = workspaces
            .get(&ws_path)
            .unwrap()
            .workers
            .get("w-1")
            .unwrap();
        assert!(worker.pr.is_some());
        assert_eq!(worker.pr.as_ref().unwrap().number, 42);
        assert!(state_dirty);
    }

    #[test]
    fn apply_pr_poll_results_ignores_missing_worker() {
        let mut workspaces = HashMap::new();
        let ws_path = PathBuf::from("/tmp/ws");
        workspaces.insert(ws_path.clone(), test_workspace("/tmp/ws", vec!["w-1"]));

        let results = vec![PrPollResult {
            worker_id: "nonexistent".to_string(),
            workspace_path: ws_path,
            pr: PrInfo {
                number: 1,
                title: "x".to_string(),
                state: "OPEN".to_string(),
                url: "https://example.com".to_string(),
            },
            is_new: true,
        }];

        let mut state_dirty = false;
        let (event_tx, _) = broadcast::channel(16);
        apply_pr_poll_results(results, &mut workspaces, &mut state_dirty, &event_tx);
        assert!(!state_dirty);
    }

    #[test]
    fn apply_pr_poll_results_ignores_missing_workspace() {
        let mut workspaces = HashMap::new();

        let results = vec![PrPollResult {
            worker_id: "w-1".to_string(),
            workspace_path: PathBuf::from("/tmp/nonexistent"),
            pr: PrInfo {
                number: 1,
                title: "x".to_string(),
                state: "OPEN".to_string(),
                url: "https://example.com".to_string(),
            },
            is_new: true,
        }];

        let mut state_dirty = false;
        let (event_tx, _) = broadcast::channel(16);
        apply_pr_poll_results(results, &mut workspaces, &mut state_dirty, &event_tx);
        assert!(!state_dirty);
    }

    #[test]
    fn apply_pr_poll_results_auto_closes_merged_pr() {
        let mut workspaces = HashMap::new();
        let ws_path = PathBuf::from("/tmp/ws");
        let ws = test_workspace("/tmp/ws", vec!["w-1"]);
        workspaces.insert(ws_path.clone(), ws);

        let results = vec![PrPollResult {
            worker_id: "w-1".to_string(),
            workspace_path: ws_path.clone(),
            pr: PrInfo {
                number: 42,
                title: "merged pr".to_string(),
                state: "MERGED".to_string(),
                url: "https://github.com/test/repo/pull/42".to_string(),
            },
            is_new: false,
        }];

        let mut state_dirty = false;
        let (event_tx, mut event_rx) = broadcast::channel(16);
        apply_pr_poll_results(results, &mut workspaces, &mut state_dirty, &event_tx);

        // Worker should be removed from workspace
        assert!(workspaces.get(&ws_path).unwrap().workers.is_empty());
        assert!(state_dirty);

        // Should have broadcast a StateChanged event
        let event = event_rx.try_recv().unwrap();
        match event {
            DaemonResponse::StateChanged { worktree_id, phase } => {
                assert_eq!(worktree_id, "w-1");
                assert_eq!(phase, WorkerPhase::Completed);
            }
            other => panic!("expected StateChanged, got {:?}", other),
        }
    }

    #[test]
    fn apply_pr_poll_results_skips_close_when_disabled() {
        let mut workspaces = HashMap::new();
        let ws_path = PathBuf::from("/tmp/ws");
        let mut ws = test_workspace("/tmp/ws", vec!["w-1"]);
        ws.close_on_pr_merge = false;
        workspaces.insert(ws_path.clone(), ws);

        let results = vec![PrPollResult {
            worker_id: "w-1".to_string(),
            workspace_path: ws_path.clone(),
            pr: PrInfo {
                number: 42,
                title: "merged pr".to_string(),
                state: "MERGED".to_string(),
                url: "https://github.com/test/repo/pull/42".to_string(),
            },
            is_new: false,
        }];

        let mut state_dirty = false;
        let (event_tx, _event_rx) = broadcast::channel(16);
        apply_pr_poll_results(results, &mut workspaces, &mut state_dirty, &event_tx);

        // Worker should still exist — not auto-closed
        let worker = workspaces
            .get(&ws_path)
            .unwrap()
            .workers
            .get("w-1")
            .expect("worker should still exist");
        assert_eq!(worker.pr.as_ref().unwrap().state, "MERGED");
        assert_eq!(worker.phase, WorkerPhase::Running);
        assert!(state_dirty);
    }

    #[test]
    fn poll_prs_background_returns_empty_for_no_repo() {
        // PrPollJob with nonexistent repo_path — gh will fail, should return empty
        let jobs = vec![PrPollJob {
            worker_id: "test".to_string(),
            branch: "swarm/test".to_string(),
            repo_path: PathBuf::from("/tmp/nonexistent-repo-12345"),
            worktree_path: PathBuf::from("/tmp/nonexistent-wt-12345"),
            had_pr: false,
            workspace_path: PathBuf::from("/tmp/ws"),
        }];
        let results = poll_prs_background(jobs);
        assert!(results.is_empty());
    }
}
