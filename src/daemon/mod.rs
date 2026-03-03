pub mod agent_supervisor;
pub mod managed_agent;
pub mod protocol;
pub mod socket_server;

use crate::core::agent::AgentKind;
use crate::core::{git, ipc, state};
use crate::core::state::{SwarmState, WorkerPhase, WorktreeState};
use crate::swarm_log;
use crate::core::state::PrInfo;
use agent_supervisor::SupervisorEvent;
use chrono::Local;
use color_eyre::Result;
use protocol::{
    AgentEventWire, DaemonRequest, DaemonResponse, WorkerInfo, WorkspaceInfo,
};
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
pub(crate) fn read_global_pid() -> Option<u32> {
    std::fs::read_to_string(ipc::global_pid_path())
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Remove the global PID file.
fn remove_pid() {
    let _ = std::fs::remove_file(ipc::global_pid_path());
}

/// Check whether a process is alive.
pub(crate) fn is_process_alive(pid: u32) -> bool {
    // signal 0 just checks existence
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

// ── Per-workspace state ──────────────────────────────────

/// State for a single workspace managed by the daemon.
struct WorkspaceState {
    path: PathBuf,
    repos: Vec<PathBuf>,
    workers: HashMap<String, ManagedWorker>,
    inbox_offset: u64,
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
    review_configs: Option<Vec<crate::core::review::ReviewConfig>>,
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
            review_slugs: self
                .review_configs
                .as_ref()
                .map(|cfgs| {
                    cfgs.iter()
                        .filter_map(|c| c.slug.clone())
                        .collect()
                })
                .unwrap_or_default(),
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
            agent: None, // daemon mode: no tmux panes
            terminals: vec![],
            summary: None,
            pr: self.pr.clone(),
            phase: self.phase.clone(),
            status,
            agent_session_status,
            review_configs: self.review_configs.clone(),
            review_parent: None,
            review_slug: None,
            review_mode: None,
            agent_pid: None,
            session_id: self.session_id.clone(),
            restart_count: Some(self.restart_count),
        }
    }
}

// ── Daemon lifecycle ─────────────────────────────────────

/// Start the swarm daemon.
pub async fn start(
    work_dir: PathBuf,
    foreground: bool,
    tcp_bind: Option<String>,
) -> Result<()> {
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
            eprintln!("[swarm] Stopping daemon (pid {})", pid);
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
                eprintln!("[swarm] Daemon did not exit, sending SIGKILL");
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
            remove_pid();
            Ok(())
        }
        Some(_) => {
            remove_pid();
            Err(color_eyre::eyre::eyre!("daemon not running (stale PID file removed)"))
        }
        None => Err(color_eyre::eyre::eyre!("daemon not running")),
    }
}

/// Restart the daemon.
pub async fn restart(
    work_dir: PathBuf,
    foreground: bool,
    tcp_bind: Option<String>,
) -> Result<()> {
    let _ = stop(&work_dir); // Ignore errors if not running
    start(work_dir, foreground, tcp_bind).await
}

/// Get daemon status.
pub fn status(_work_dir: &Path) -> Result<()> {
    match read_global_pid() {
        Some(pid) if is_process_alive(pid) => {
            println!("daemon running (pid {})", pid);
            println!(
                "socket: {}",
                ipc::global_socket_path().display()
            );
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
    let repos = tokio::task::spawn_blocking(move || {
        git::detect_repos(&bg_canonical).unwrap_or_default()
    })
    .await
    .unwrap_or_default();

    // Load existing workers from state.json
    let mut workers = HashMap::new();
    if let Ok(Some(existing_state)) = state::load_state(&canonical) {
        for wt in &existing_state.worktrees {
            if wt.phase.is_active() {
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
                        review_configs: wt.review_configs.clone(),
                    },
                );
            }
        }
    }

    let worker_count = workers.len();
    workspaces.insert(
        canonical.clone(),
        WorkspaceState {
            path: canonical.clone(),
            repos,
            workers,
            inbox_offset: 0,
        },
    );

    eprintln!(
        "[swarm] Registered workspace: {} ({} repos, {} existing workers)",
        canonical.display(),
        workspaces[&canonical].repos.len(),
        worker_count,
    );

    Ok(canonical)
}

// ── Main daemon loop ─────────────────────────────────────

/// The main daemon event loop.
async fn run_daemon(
    initial_work_dir: Option<PathBuf>,
    tcp_bind: Option<String>,
    auth_token: Option<String>,
) -> Result<()> {
    write_pid()?;
    eprintln!("[swarm] Daemon starting (pid {})", std::process::id());

    // Set up signal handlers
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    // Event broadcast channel for subscribers
    let (event_tx, _) = broadcast::channel::<(String, AgentEventWire)>(1024);

    // Supervisor event channel
    let (supervisor_tx, mut supervisor_rx) = mpsc::unbounded_channel::<SupervisorEvent>();

    // Start the socket server on global path
    let (mut request_rx, _socket_handle) =
        socket_server::start(event_tx.clone(), tcp_bind, auth_token)?;

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

    // PR polling
    let pr_poll_interval = std::time::Duration::from_secs(30);
    let mut pr_poll = tokio::time::interval(pr_poll_interval);
    pr_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Daemon is ready to accept connections immediately.
    eprintln!("[swarm] Daemon ready, accepting connections.");

    // Register initial workspace in a background task — detect_repos is expensive
    // and we don't want to block the select loop from processing Ping/Subscribe.
    let mut pending_register: Option<tokio::task::JoinHandle<Option<(PathBuf, WorkspaceState)>>> =
        if let Some(ref wd) = initial_work_dir {
            let wd = wd.clone();
            Some(tokio::task::spawn(async move {
                let canonical =
                    std::fs::canonicalize(&wd).unwrap_or_else(|_| wd.to_path_buf());
                let bg = canonical.clone();
                let repos = tokio::task::spawn_blocking(move || {
                    git::detect_repos(&bg).unwrap_or_default()
                })
                .await
                .unwrap_or_default();
                let mut workers = HashMap::new();
                if let Ok(Some(existing_state)) = state::load_state(&canonical) {
                    for wt in &existing_state.worktrees {
                        if wt.phase.is_active() {
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
                                    review_configs: wt.review_configs.clone(),
                                },
                            );
                        }
                    }
                }
                let worker_count = workers.len();
                let repo_count = repos.len();
                let ws = WorkspaceState {
                    path: canonical.clone(),
                    repos,
                    workers,
                    inbox_offset: 0,
                };
                eprintln!(
                    "[swarm] Registered workspace: {} ({} repos, {} existing workers)",
                    canonical.display(),
                    repo_count,
                    worker_count,
                );
                Some((canonical, ws))
            }))
        } else {
            None
        };

    loop {
        // Check if deferred workspace registration completed
        if let Some(ref task) = pending_register {
            if task.is_finished() {
                if let Some(task) = pending_register.take() {
                    if let Ok(Some((path, ws))) = task.await {
                        workspaces.insert(path, ws);
                    }
                }
            }
        }

        tokio::select! {
            // Signal handlers
            _ = sigterm.recv() => {
                eprintln!("[swarm] Received SIGTERM, shutting down...");
                break;
            }
            _ = sigint.recv() => {
                eprintln!("[swarm] Received SIGINT, shutting down...");
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

                                // Emit phase change event
                                let _ = ipc::emit_event(&ws.path, &ipc::SwarmEvent::PhaseChanged {
                                    worktree: worktree_id.clone(),
                                    from: old_phase,
                                    to: phase,
                                    timestamp: Local::now(),
                                });
                                break;
                            }
                        }
                    }
                    SupervisorEvent::AgentEvent { .. } => {
                        // Events are already broadcast via the event_tx channel
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
                        ).await;
                    }
                }
            }

            // PR polling
            _ = pr_poll.tick() => {
                for ws in workspaces.values_mut() {
                    poll_prs(&mut ws.workers, &ws.path, &mut state_dirty);
                }
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
    eprintln!("[swarm] Interrupting active agents...");

    // Final state save
    save_all_workspace_states(&workspaces);
    remove_pid();
    eprintln!("[swarm] Daemon stopped.");

    Ok(())
}

// ── Request handling ─────────────────────────────────────

/// Handle a daemon request.
async fn handle_request(
    request: DaemonRequest,
    resp_tx: &mpsc::UnboundedSender<DaemonResponse>,
    workspaces: &mut HashMap<PathBuf, WorkspaceState>,
    event_tx: &broadcast::Sender<(String, AgentEventWire)>,
    supervisor_tx: &mpsc::UnboundedSender<SupervisorEvent>,
    state_dirty: &mut bool,
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
                eprintln!("[swarm] Unregistered workspace: {}", canonical.display());
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
            review_configs,
            workspace,
        } => {
            let kind = match AgentKind::from_str(&agent) {
                Some(k) => k,
                None => {
                    let _ = resp_tx.send(DaemonResponse::Error {
                        message: format!("unknown agent: {}", agent),
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

            // Generate IDs (short UUID suffix, e.g. "hive-a1b2")
            let short_id = &uuid::Uuid::new_v4().to_string()[..4];
            let repo_name = git::repo_name(&repo_path);
            let worktree_id = format!("{}-{}", repo_name, short_id);
            let branch = git::generate_branch_name(&prompt, short_id);

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
                review_configs: review_configs.clone(),
            };

            ws.workers.insert(worktree_id.clone(), worker);
            *state_dirty = true;

            // Spawn the agent in a background task
            let wt_id = worktree_id.clone();
            let wt_path = worktree_path.clone();
            let wd = work_dir.clone();
            let ev_tx = event_tx.clone();
            let sv_tx = supervisor_tx.clone();
            let agent_prompt = prompt.clone();
            let agent_kind = kind.clone();

            // Create a message channel for follow-ups
            let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<String>();
            if let Some(w) = ws.workers.get_mut(&wt_id) {
                w.message_tx = Some(msg_tx);
            }

            tokio::spawn(async move {
                // Spawn the initial agent
                let handle_result = agent_supervisor::spawn_agent(
                    agent_supervisor::SpawnAgentOpts {
                        worktree_id: &wt_id,
                        kind: agent_kind.clone(),
                        prompt: &agent_prompt,
                        worktree_path: &wt_path,
                        work_dir: &wd,
                        resume_session_id: None,
                        dangerously_skip_permissions: true,
                        event_tx: ev_tx.clone(),
                    },
                )
                .await;

                let mut handle = match handle_result {
                    Ok(h) => h,
                    Err(e) => {
                        swarm_log!("[daemon] Failed to spawn agent {}: {}", wt_id, e);
                        let _ = sv_tx.send(SupervisorEvent::PhaseChanged {
                            worktree_id: wt_id,
                            phase: WorkerPhase::Failed,
                            session_id: None,
                        });
                        return;
                    }
                };

                // Signal that we're running
                let _ = sv_tx.send(SupervisorEvent::PhaseChanged {
                    worktree_id: wt_id.clone(),
                    phase: WorkerPhase::Running,
                    session_id: None,
                });

                // Initial event loop
                let mut restart_count = 0u32;
                let (phase, _session_id) = agent_supervisor::agent_event_loop(
                    &mut handle,
                    agent_supervisor::EventLoopOpts {
                        supervisor_tx: &sv_tx,
                        work_dir: &wd,
                        restart_count: &mut restart_count,
                        kind: agent_kind.clone(),
                        prompt: &agent_prompt,
                        worktree_path: &wt_path,
                        dangerously_skip_permissions: true,
                    },
                )
                .await;

                // If the agent is waiting, listen for follow-up messages
                if phase == WorkerPhase::Waiting {
                    loop {
                        let message = match msg_rx.recv().await {
                            Some(msg) => msg,
                            None => break,
                        };

                        if let Err(e) = handle.agent.send_message(&message).await {
                            swarm_log!(
                                "[daemon] Failed to send message to {}: {}",
                                wt_id,
                                e
                            );
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

                        let (new_phase, _) = agent_supervisor::agent_event_loop(
                            &mut handle,
                            agent_supervisor::EventLoopOpts {
                                supervisor_tx: &sv_tx,
                                work_dir: &wd,
                                restart_count: &mut restart_count,
                                kind: agent_kind.clone(),
                                prompt: &agent_prompt,
                                worktree_path: &wt_path,
                                dangerously_skip_permissions: true,
                            },
                        )
                        .await;

                        if new_phase != WorkerPhase::Waiting {
                            break;
                        }
                    }
                }
            });

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

        DaemonRequest::Review { worktree_id, slug } => {
            swarm_log!(
                "[daemon] Review request for {} (slug: {:?})",
                worktree_id,
                slug
            );
            let _ = resp_tx.send(DaemonResponse::Ok { data: None });
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
                let paths: Vec<String> = workspaces
                    .keys()
                    .map(|p| p.display().to_string())
                    .collect();
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
        Some(name) => {
            repos
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
                })
        }
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
        swarm_log!("[daemon] Failed to save state for {}: {}", work_dir.display(), e);
    }
}

/// Poll PRs for all active workers in a workspace.
fn poll_prs(
    workers: &mut HashMap<String, ManagedWorker>,
    work_dir: &Path,
    state_dirty: &mut bool,
) {
    for worker in workers.values_mut() {
        if !worker.phase.is_active() {
            continue;
        }

        let output = std::process::Command::new("gh")
            .args([
                "pr",
                "list",
                "--head",
                &worker.branch,
                "--state",
                "all",
                "--json",
                "number,title,state,url",
                "--limit",
                "1",
            ])
            .current_dir(&worker.repo_path)
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

                let is_new = worker.pr.is_none();
                if is_new {
                    swarm_log!(
                        "[daemon] PR detected for {}: #{} \"{}\"",
                        worker.id,
                        new_pr.number,
                        new_pr.title
                    );
                    let _ = ipc::emit_event(
                        work_dir,
                        &ipc::SwarmEvent::PrDetected {
                            worktree: worker.id.clone(),
                            pr_url: new_pr.url.clone(),
                            pr_title: new_pr.title.clone(),
                            pr_number: new_pr.number,
                            timestamp: Local::now(),
                        },
                    );
                }

                worker.pr = Some(new_pr);
                *state_dirty = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::review::ReviewConfig;

    /// Build a ManagedWorker with sensible defaults for testing.
    fn test_worker(id: &str, review_configs: Option<Vec<ReviewConfig>>) -> ManagedWorker {
        ManagedWorker {
            id: id.to_string(),
            branch: format!("swarm/{}", id),
            prompt: "test prompt".to_string(),
            kind: AgentKind::ClaudeTui,
            repo_path: PathBuf::from("/tmp/repo"),
            worktree_path: PathBuf::from(format!("/tmp/wt/{}", id)),
            phase: WorkerPhase::Running,
            session_id: None,
            restart_count: 0,
            pr: None,
            created_at: Local::now(),
            message_tx: None,
            review_configs,
        }
    }

    /// Build channels needed for handle_request tests.
    fn test_harness() -> (
        mpsc::UnboundedReceiver<DaemonResponse>,
        mpsc::UnboundedSender<DaemonResponse>,
        HashMap<PathBuf, WorkspaceState>,
        broadcast::Sender<(String, AgentEventWire)>,
        mpsc::UnboundedSender<SupervisorEvent>,
    ) {
        let (resp_tx, resp_rx) = mpsc::unbounded_channel();
        let workspaces = HashMap::new();
        let (event_tx, _) = broadcast::channel(16);
        let (supervisor_tx, _) = mpsc::unbounded_channel();
        (resp_rx, resp_tx, workspaces, event_tx, supervisor_tx)
    }

    /// Create a test workspace with optional workers.
    fn test_workspace(path: &str, workers: Vec<(&str, Option<Vec<ReviewConfig>>)>) -> WorkspaceState {
        let mut ws_workers = HashMap::new();
        for (id, cfgs) in workers {
            ws_workers.insert(id.to_string(), test_worker(id, cfgs));
        }
        WorkspaceState {
            path: PathBuf::from(path),
            repos: vec![],
            workers: ws_workers,
            inbox_offset: 0,
        }
    }

    #[test]
    fn to_worker_info_extracts_review_slugs() {
        let cfgs = vec![
            ReviewConfig {
                prompt: crate::core::review::ReviewPrompt::BuiltIn {
                    slug: "code-review".into(),
                },
                agent: None,
                extra_instructions: None,
                slug: Some("code-review".into()),
                mode: crate::core::review::ReviewMode::Review,
            },
            ReviewConfig {
                prompt: crate::core::review::ReviewPrompt::BuiltIn {
                    slug: "security-audit".into(),
                },
                agent: None,
                extra_instructions: None,
                slug: Some("security-audit".into()),
                mode: crate::core::review::ReviewMode::Review,
            },
        ];
        let worker = test_worker("hive-abc", Some(cfgs));
        let info = worker.to_worker_info();
        assert_eq!(info.review_slugs, vec!["code-review", "security-audit"]);
    }

    #[test]
    fn to_worker_info_no_review_configs() {
        let worker = test_worker("hive-def", None);
        let info = worker.to_worker_info();
        assert!(info.review_slugs.is_empty());
    }

    #[tokio::test]
    async fn handle_request_ping() {
        let (mut resp_rx, resp_tx, mut workspaces, event_tx, supervisor_tx) = test_harness();
        let mut state_dirty = false;

        handle_request(
            DaemonRequest::Ping,
            &resp_tx,
            &mut workspaces,
            &event_tx,
            &supervisor_tx,
            &mut state_dirty,
        )
        .await;

        let resp = resp_rx.try_recv().unwrap();
        assert!(matches!(resp, DaemonResponse::Ok { data: None }));
    }

    #[tokio::test]
    async fn handle_request_list_workers_empty() {
        let (mut resp_rx, resp_tx, mut workspaces, event_tx, supervisor_tx) = test_harness();
        let mut state_dirty = false;

        handle_request(
            DaemonRequest::ListWorkers { workspace: None },
            &resp_tx,
            &mut workspaces,
            &event_tx,
            &supervisor_tx,
            &mut state_dirty,
        )
        .await;

        let resp = resp_rx.try_recv().unwrap();
        match resp {
            DaemonResponse::Workers { workers } => assert!(workers.is_empty()),
            other => panic!("expected Workers, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn handle_request_list_workers_with_reviews() {
        let (mut resp_rx, resp_tx, mut workspaces, event_tx, supervisor_tx) = test_harness();
        let mut state_dirty = false;

        let cfgs = vec![ReviewConfig {
            prompt: crate::core::review::ReviewPrompt::BuiltIn {
                slug: "code-review".into(),
            },
            agent: None,
            extra_instructions: None,
            slug: Some("code-review".into()),
            mode: crate::core::review::ReviewMode::Review,
        }];
        workspaces.insert(
            PathBuf::from("/tmp/test"),
            test_workspace("/tmp/test", vec![("hive-1", Some(cfgs))]),
        );

        handle_request(
            DaemonRequest::ListWorkers { workspace: None },
            &resp_tx,
            &mut workspaces,
            &event_tx,
            &supervisor_tx,
            &mut state_dirty,
        )
        .await;

        let resp = resp_rx.try_recv().unwrap();
        match resp {
            DaemonResponse::Workers { workers } => {
                assert_eq!(workers.len(), 1);
                assert_eq!(workers[0].review_slugs, vec!["code-review"]);
            }
            other => panic!("expected Workers, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn handle_request_list_workers_filtered_by_workspace() {
        let (mut resp_rx, resp_tx, mut workspaces, event_tx, supervisor_tx) = test_harness();
        let mut state_dirty = false;

        workspaces.insert(
            PathBuf::from("/tmp/ws1"),
            test_workspace("/tmp/ws1", vec![("hive-1", None)]),
        );
        workspaces.insert(
            PathBuf::from("/tmp/ws2"),
            test_workspace("/tmp/ws2", vec![("hive-2", None), ("hive-3", None)]),
        );

        // List all
        handle_request(
            DaemonRequest::ListWorkers { workspace: None },
            &resp_tx,
            &mut workspaces,
            &event_tx,
            &supervisor_tx,
            &mut state_dirty,
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

        handle_request(
            DaemonRequest::CloseWorker {
                worktree_id: "nonexistent".into(),
            },
            &resp_tx,
            &mut workspaces,
            &event_tx,
            &supervisor_tx,
            &mut state_dirty,
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

        workspaces.insert(
            PathBuf::from("/tmp/ws1"),
            test_workspace("/tmp/ws1", vec![("hive-1", None)]),
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

        workspaces.insert(
            PathBuf::from("/tmp/ws1"),
            test_workspace("/tmp/ws1", vec![("hive-1", None)]),
        );
        workspaces.insert(
            PathBuf::from("/tmp/ws2"),
            test_workspace("/tmp/ws2", vec![("hive-2", None)]),
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
}
