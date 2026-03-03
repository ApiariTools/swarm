use super::managed_agent::{self, ManagedAgent, SpawnOptions};
use super::protocol::AgentEventWire;
use crate::agent_tui::events::EventLogger;
use crate::core::agent::AgentKind;
use crate::core::state::WorkerPhase;
use crate::swarm_log;
use std::path::Path;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

/// Maximum number of automatic restart attempts before marking as Failed.
const MAX_RESTARTS: u32 = 3;

/// A handle to a supervised agent with its communication channels.
pub struct AgentHandle {
    pub worktree_id: String,
    pub agent: Box<dyn ManagedAgent>,
    pub event_tx: broadcast::Sender<(String, AgentEventWire)>,
    pub logger: EventLogger,
}

/// Messages sent from the supervisor to the daemon main loop.
#[derive(Debug, Clone)]
pub enum SupervisorEvent {
    /// Agent phase changed.
    PhaseChanged {
        worktree_id: String,
        phase: WorkerPhase,
        session_id: Option<String>,
    },
    /// Agent produced an event (for broadcasting to subscribers).
    #[allow(dead_code)]
    AgentEvent {
        worktree_id: String,
        event: AgentEventWire,
    },
}

/// Options for spawning a new agent.
pub struct SpawnAgentOpts<'a> {
    pub worktree_id: &'a str,
    pub kind: AgentKind,
    pub prompt: &'a str,
    pub worktree_path: &'a Path,
    pub work_dir: &'a Path,
    pub resume_session_id: Option<String>,
    pub dangerously_skip_permissions: bool,
    pub event_tx: broadcast::Sender<(String, AgentEventWire)>,
}

/// Spawn a new agent and return a handle for interacting with it.
pub async fn spawn_agent(opts: SpawnAgentOpts<'_>) -> color_eyre::Result<AgentHandle> {
    let max_turns = match opts.kind {
        AgentKind::Claude => Some(50),
        _ => None,
    };

    let spawn_opts = SpawnOptions {
        kind: opts.kind,
        prompt: opts.prompt.to_string(),
        working_dir: opts.worktree_path.to_path_buf(),
        dangerously_skip_permissions: opts.dangerously_skip_permissions,
        resume_session_id: opts.resume_session_id,
        max_turns,
    };

    let agent = managed_agent::spawn_managed_agent(spawn_opts).await?;

    let event_log_path = opts
        .work_dir
        .join(".swarm")
        .join("agents")
        .join(opts.worktree_id)
        .join("events.jsonl");
    let logger = EventLogger::new(event_log_path);

    Ok(AgentHandle {
        worktree_id: opts.worktree_id.to_string(),
        agent,
        event_tx: opts.event_tx,
        logger,
    })
}

/// Options for the agent event loop.
pub struct EventLoopOpts<'a> {
    pub supervisor_tx: &'a mpsc::UnboundedSender<SupervisorEvent>,
    pub work_dir: &'a Path,
    pub restart_count: &'a mut u32,
    pub kind: AgentKind,
    pub prompt: &'a str,
    pub worktree_path: &'a Path,
    pub dangerously_skip_permissions: bool,
}

/// Run the event loop for a supervised agent. Drains events, logs them,
/// broadcasts to subscribers, and handles crash recovery.
///
/// Returns the final phase and session_id when the agent finishes.
pub async fn agent_event_loop(
    handle: &mut AgentHandle,
    opts: EventLoopOpts<'_>,
) -> (WorkerPhase, Option<String>) {
    let EventLoopOpts {
        supervisor_tx,
        work_dir,
        restart_count,
        kind,
        prompt,
        worktree_path,
        dangerously_skip_permissions,
    } = opts;
    loop {
        // Drain events from the current agent
        let result = drain_agent_events(handle, supervisor_tx, work_dir).await;

        match result {
            AgentExitReason::Completed(session_id) => {
                // Agent finished normally — transition to Waiting or Completed
                if handle.agent.accepts_input() {
                    // Write agent-status file
                    write_agent_status(work_dir, &handle.worktree_id, "waiting");
                    let _ = supervisor_tx.send(SupervisorEvent::PhaseChanged {
                        worktree_id: handle.worktree_id.clone(),
                        phase: WorkerPhase::Waiting,
                        session_id: session_id.clone(),
                    });
                    return (WorkerPhase::Waiting, session_id);
                } else {
                    return (WorkerPhase::Completed, session_id);
                }
            }
            AgentExitReason::Crashed(error) => {
                *restart_count += 1;
                swarm_log!(
                    "[daemon] Agent {} crashed (attempt {}/{}): {}",
                    handle.worktree_id,
                    restart_count,
                    MAX_RESTARTS,
                    error
                );
                handle.logger.log_error(&format!(
                    "Agent crashed (attempt {}/{}): {}",
                    restart_count, MAX_RESTARTS, error
                ));

                if *restart_count > MAX_RESTARTS {
                    swarm_log!(
                        "[daemon] Agent {} exceeded max restarts, marking as Failed",
                        handle.worktree_id
                    );
                    return (WorkerPhase::Failed, handle.agent.session_id().map(String::from));
                }

                // Exponential backoff: 2^restart_count seconds, max 60
                let delay_secs = std::cmp::min(2u64.pow(*restart_count), 60);
                swarm_log!(
                    "[daemon] Restarting agent {} in {}s with session resume",
                    handle.worktree_id,
                    delay_secs
                );
                tokio::time::sleep(Duration::from_secs(delay_secs)).await;

                // Re-spawn with session resume
                let resume_id = handle.agent.session_id().map(String::from);
                let max_turns = match kind {
                    AgentKind::Claude => Some(50),
                    _ => None,
                };

                let new_opts = SpawnOptions {
                    kind: kind.clone(),
                    prompt: prompt.to_string(),
                    working_dir: worktree_path.to_path_buf(),
                    dangerously_skip_permissions,
                    resume_session_id: resume_id,
                    max_turns,
                };

                match managed_agent::spawn_managed_agent(new_opts).await {
                    Ok(new_agent) => {
                        handle.agent = new_agent;
                        let _ = supervisor_tx.send(SupervisorEvent::PhaseChanged {
                            worktree_id: handle.worktree_id.clone(),
                            phase: WorkerPhase::Running,
                            session_id: handle.agent.session_id().map(String::from),
                        });
                        // Continue the loop — drain events from the new agent
                        continue;
                    }
                    Err(e) => {
                        swarm_log!(
                            "[daemon] Failed to restart agent {}: {}",
                            handle.worktree_id,
                            e
                        );
                        handle
                            .logger
                            .log_error(&format!("Failed to restart: {}", e));
                        return (WorkerPhase::Failed, None);
                    }
                }
            }
        }
    }
}

/// Why the agent exited its event loop.
enum AgentExitReason {
    /// Normal completion with optional session_id.
    Completed(Option<String>),
    /// Crashed with error message.
    Crashed(String),
}

/// Drain events from the agent until it finishes or errors.
async fn drain_agent_events(
    handle: &mut AgentHandle,
    supervisor_tx: &mpsc::UnboundedSender<SupervisorEvent>,
    work_dir: &std::path::Path,
) -> AgentExitReason {
    write_agent_status(work_dir, &handle.worktree_id, "running");

    loop {
        match handle.agent.next_event().await {
            Ok(Some(event)) => {
                // Log the event
                log_agent_event(&handle.logger, &event);

                // Broadcast to subscribers
                let _ = handle
                    .event_tx
                    .send((handle.worktree_id.clone(), event.clone()));

                // Notify daemon of the event
                let _ = supervisor_tx.send(SupervisorEvent::AgentEvent {
                    worktree_id: handle.worktree_id.clone(),
                    event: event.clone(),
                });

                // If this was a SessionResult, capture the completion
                if let AgentEventWire::SessionResult { session_id, .. } = &event {
                    return AgentExitReason::Completed(session_id.clone());
                }
            }
            Ok(None) => {
                // EOF — agent process exited
                let session_id = handle.agent.session_id().map(String::from);
                return AgentExitReason::Completed(session_id);
            }
            Err(e) => {
                return AgentExitReason::Crashed(e.to_string());
            }
        }
    }
}

/// Log an AgentEventWire to the event logger.
fn log_agent_event(logger: &EventLogger, event: &AgentEventWire) {
    match event {
        AgentEventWire::TextDelta { text } => {
            logger.log_text(text);
        }
        AgentEventWire::ToolUse { tool, input } => {
            logger.log_tool_use(tool, input);
        }
        AgentEventWire::ToolResult { output, is_error } => {
            logger.log_tool_result("", output, *is_error);
        }
        AgentEventWire::SessionResult {
            turns,
            cost_usd,
            session_id,
        } => {
            logger.log_session_result(*turns, *cost_usd, session_id.as_deref());
        }
        AgentEventWire::Error { message } => {
            logger.log_error(message);
        }
        AgentEventWire::ThinkingDelta { .. }
        | AgentEventWire::TurnComplete
        | AgentEventWire::SessionWaiting { .. } => {}
    }
}

/// Write the agent status file for hive to read.
fn write_agent_status(work_dir: &std::path::Path, worktree_id: &str, status: &str) {
    crate::core::agent_status::write_agent_status(work_dir, worktree_id, status);
}
