use super::event_logger::EventLogger;
use super::managed_agent::{self, ManagedAgent, SpawnOptions};
use super::protocol::{AgentEventWire, DaemonResponse};
use crate::core::agent::AgentKind;
use crate::core::state::WorkerPhase;
use std::path::Path;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

/// Maximum number of automatic restart attempts before marking as Failed.
const MAX_RESTARTS: u32 = 3;

/// After sending a follow-up message, if no event arrives within this duration,
/// consider the agent stalled and return to `Waiting`.
const SEND_FIRST_EVENT_TIMEOUT: Duration = Duration::from_secs(120); // 2 min

/// Maximum total time for an `agent_event_loop()` call after a follow-up message.
/// If the agent produces events but never completes within this window, it is
/// considered stalled.
const SEND_TOTAL_TIMEOUT: Duration = Duration::from_secs(600); // 10 min

/// A handle to a supervised agent with its communication channels.
pub struct AgentHandle {
    pub worktree_id: String,
    pub agent: Box<dyn ManagedAgent>,
    pub event_tx: broadcast::Sender<DaemonResponse>,
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
    pub event_tx: broadcast::Sender<DaemonResponse>,
}

/// Spawn a new agent and return a handle for interacting with it.
pub async fn spawn_agent(opts: SpawnAgentOpts<'_>) -> color_eyre::Result<AgentHandle> {
    let spawn_opts = SpawnOptions {
        kind: opts.kind,
        prompt: opts.prompt.to_string(),
        working_dir: opts.worktree_path.to_path_buf(),
        dangerously_skip_permissions: opts.dangerously_skip_permissions,
        resume_session_id: opts.resume_session_id,
        max_turns: None,
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
    // Initial event loop — no first-event timeout. Follow-up calls from
    // the Waiting loop use `agent_event_loop_with_timeouts` instead.
    agent_event_loop_impl(handle, opts, None).await
}

/// Like [`agent_event_loop`] but applies stall-detection timeouts.
///
/// Used for follow-up messages in the `Waiting` loop where the agent has already
/// been started and we want to detect unresponsive agents.
///
/// - `first_event_timeout`: max time to wait for the first event after sending a
///   message. Defaults to [`SEND_FIRST_EVENT_TIMEOUT`].
/// - `total_timeout`: max total time for the entire event loop. Defaults to
///   [`SEND_TOTAL_TIMEOUT`].
pub async fn agent_event_loop_with_timeouts(
    handle: &mut AgentHandle,
    opts: EventLoopOpts<'_>,
    first_event_timeout: Option<Duration>,
    total_timeout: Option<Duration>,
) -> (WorkerPhase, Option<String>) {
    let first_ev = first_event_timeout.unwrap_or(SEND_FIRST_EVENT_TIMEOUT);
    let total = total_timeout.unwrap_or(SEND_TOTAL_TIMEOUT);
    let work_dir = opts.work_dir.to_path_buf();
    let supervisor_tx = opts.supervisor_tx.clone();

    match tokio::time::timeout(total, agent_event_loop_impl(handle, opts, Some(first_ev))).await {
        Ok(result) => result,
        Err(_elapsed) => {
            let timeout_display = format_duration(total);
            tracing::warn!(
                worker_id = %handle.worktree_id,
                "Agent stalled after message — total response timeout ({}) exceeded",
                timeout_display,
            );
            handle.logger.log_error(&format!(
                "Agent stalled — total response timeout ({}) exceeded",
                timeout_display,
            ));
            let session_id = handle.agent.session_id().map(String::from);
            write_agent_status(&work_dir, &handle.worktree_id, "waiting");
            let _ = supervisor_tx.send(SupervisorEvent::PhaseChanged {
                worktree_id: handle.worktree_id.clone(),
                phase: WorkerPhase::Waiting,
                session_id: session_id.clone(),
            });
            (WorkerPhase::Waiting, session_id)
        }
    }
}

/// Core event loop implementation with optional first-event timeout.
///
/// Used by both `agent_event_loop` (no timeouts) and
/// `agent_event_loop_with_timeouts` (wrapped with total timeout).
async fn agent_event_loop_impl(
    handle: &mut AgentHandle,
    opts: EventLoopOpts<'_>,
    first_event_timeout: Option<Duration>,
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
    // Only apply first-event timeout on the initial drain, not on crash-restart.
    let mut current_first_event_timeout = first_event_timeout;
    loop {
        let result =
            drain_agent_events(handle, supervisor_tx, work_dir, current_first_event_timeout).await;
        current_first_event_timeout = None;

        match result {
            AgentExitReason::Completed(session_id) => {
                if handle.agent.accepts_input() {
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
            AgentExitReason::Stalled => {
                let session_id = handle.agent.session_id().map(String::from);
                write_agent_status(work_dir, &handle.worktree_id, "waiting");
                let _ = supervisor_tx.send(SupervisorEvent::PhaseChanged {
                    worktree_id: handle.worktree_id.clone(),
                    phase: WorkerPhase::Waiting,
                    session_id: session_id.clone(),
                });
                return (WorkerPhase::Waiting, session_id);
            }
            AgentExitReason::Crashed(error) => {
                *restart_count += 1;
                tracing::warn!(
                    worker_id = %handle.worktree_id,
                    attempt = *restart_count,
                    max = MAX_RESTARTS,
                    error = %error,
                    "Agent crashed",
                );
                handle.logger.log_error(&format!(
                    "Agent crashed (attempt {}/{}): {}",
                    restart_count, MAX_RESTARTS, error
                ));

                if *restart_count > MAX_RESTARTS {
                    tracing::error!(worker_id = %handle.worktree_id, "Agent exceeded max restarts, marking as Failed");
                    return (
                        WorkerPhase::Failed,
                        handle.agent.session_id().map(String::from),
                    );
                }

                let delay_secs = std::cmp::min(2u64.pow(*restart_count), 60);
                tracing::info!(worker_id = %handle.worktree_id, delay_secs, "Restarting agent with session resume");
                tokio::time::sleep(Duration::from_secs(delay_secs)).await;

                let resume_id = handle.agent.session_id().map(String::from);
                let new_opts = SpawnOptions {
                    kind: kind.clone(),
                    prompt: prompt.to_string(),
                    working_dir: worktree_path.to_path_buf(),
                    dangerously_skip_permissions,
                    resume_session_id: resume_id,
                    max_turns: None,
                };

                match managed_agent::spawn_managed_agent(new_opts).await {
                    Ok(new_agent) => {
                        handle.agent = new_agent;
                        let _ = supervisor_tx.send(SupervisorEvent::PhaseChanged {
                            worktree_id: handle.worktree_id.clone(),
                            phase: WorkerPhase::Running,
                            session_id: handle.agent.session_id().map(String::from),
                        });
                        continue;
                    }
                    Err(e) => {
                        tracing::error!(worker_id = %handle.worktree_id, error = %e, "Failed to restart agent");
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
    /// Agent stalled — no response or took too long after a follow-up message.
    Stalled,
}

/// Drain events from the agent until it finishes or errors.
///
/// If `first_event_timeout` is `Some(duration)`, the *first* `next_event()` call
/// is wrapped with `tokio::time::timeout`. If it fires before any event arrives,
/// we return `AgentExitReason::Stalled` so the caller can transition back to
/// `Waiting` without killing the agent.
async fn drain_agent_events(
    handle: &mut AgentHandle,
    supervisor_tx: &mpsc::UnboundedSender<SupervisorEvent>,
    work_dir: &std::path::Path,
    first_event_timeout: Option<Duration>,
) -> AgentExitReason {
    write_agent_status(work_dir, &handle.worktree_id, "running");
    let mut event_count: u64 = 0;

    tracing::debug!(worker_id = %handle.worktree_id, "Waiting for first event");

    loop {
        // Apply first-event timeout only on the very first iteration.
        let event_result = if event_count == 0
            && let Some(timeout_dur) = first_event_timeout
        {
            match tokio::time::timeout(timeout_dur, handle.agent.next_event()).await {
                Ok(inner) => inner,
                Err(_elapsed) => {
                    let timeout_display = format_duration(timeout_dur);
                    tracing::warn!(
                        worker_id = %handle.worktree_id,
                        "Agent stalled after message — no response within {}",
                        timeout_display,
                    );
                    handle.logger.log_error(&format!(
                        "Agent stalled — no response within {} after message",
                        timeout_display,
                    ));
                    return AgentExitReason::Stalled;
                }
            }
        } else {
            handle.agent.next_event().await
        };

        match event_result {
            Ok(Some(event)) => {
                event_count += 1;

                if event_count <= 3 || event_count.is_multiple_of(50) {
                    tracing::debug!(
                        worker_id = %handle.worktree_id,
                        event_count,
                        event_type = ?std::mem::discriminant(&event),
                        "Agent event",
                    );
                }

                // Log the event
                log_agent_event(&handle.logger, &event);

                // Broadcast to subscribers
                let _ = handle.event_tx.send(DaemonResponse::AgentEvent {
                    worktree_id: handle.worktree_id.clone(),
                    event: event.clone(),
                });

                // Notify daemon of the event
                let _ = supervisor_tx.send(SupervisorEvent::AgentEvent {
                    worktree_id: handle.worktree_id.clone(),
                    event: event.clone(),
                });

                // If this was a SessionResult, capture the completion
                if let AgentEventWire::SessionResult { session_id, .. } = &event {
                    tracing::info!(worker_id = %handle.worktree_id, event_count, "Agent completed with SessionResult");
                    return AgentExitReason::Completed(session_id.clone());
                }
            }
            Ok(None) => {
                // EOF — agent process exited
                if event_count == 0 {
                    // Capture stderr for diagnostics
                    let stderr = handle.agent.wait_for_stderr().await;
                    let stderr_msg = stderr
                        .as_deref()
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .unwrap_or("(no stderr)");
                    tracing::warn!(worker_id = %handle.worktree_id, stderr = %stderr_msg, "Agent exited with zero events");
                    handle.logger.log_error(&format!(
                        "Agent process exited without producing any events. stderr: {}",
                        stderr_msg
                    ));
                } else {
                    tracing::debug!(worker_id = %handle.worktree_id, event_count, "Agent EOF (no SessionResult)");
                }
                let session_id = handle.agent.session_id().map(String::from);
                return AgentExitReason::Completed(session_id);
            }
            Err(e) => {
                tracing::error!(worker_id = %handle.worktree_id, event_count, error = %e, "Agent errored");
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

/// Format a `Duration` for human-readable log output.
/// Uses seconds for >= 1s, milliseconds otherwise.
fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs > 0 {
        format!("{}s", secs)
    } else {
        format!("{}ms", d.as_millis())
    }
}

/// Write the agent status file for hive to read.
fn write_agent_status(work_dir: &std::path::Path, worktree_id: &str, status: &str) {
    let status_dir = work_dir.join(".swarm").join("agent-status");
    let _ = std::fs::create_dir_all(&status_dir);
    let _ = std::fs::write(status_dir.join(worktree_id), status);
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// Mock agent that returns a predefined sequence of events.
    struct MockAgent {
        events: Arc<Mutex<VecDeque<Result<Option<AgentEventWire>, color_eyre::Report>>>>,
        session_id: Option<String>,
        accepts: bool,
        finished: bool,
    }

    impl MockAgent {
        fn from_events(events: Vec<Option<AgentEventWire>>) -> Self {
            let queue: VecDeque<_> = events.into_iter().map(Ok).collect();
            Self {
                events: Arc::new(Mutex::new(queue)),
                session_id: None,
                accepts: false,
                finished: false,
            }
        }

        fn from_results(results: Vec<Result<Option<AgentEventWire>, color_eyre::Report>>) -> Self {
            Self {
                events: Arc::new(Mutex::new(results.into())),
                session_id: None,
                accepts: false,
                finished: false,
            }
        }
    }

    #[async_trait]
    impl ManagedAgent for MockAgent {
        fn kind(&self) -> AgentKind {
            AgentKind::Claude
        }

        async fn next_event(&mut self) -> color_eyre::Result<Option<AgentEventWire>> {
            let mut events = self.events.lock().unwrap();
            match events.pop_front() {
                Some(result) => {
                    if let Ok(None) = &result {
                        self.finished = true;
                    }
                    if result.is_err() {
                        self.finished = true;
                    }
                    result
                }
                None => {
                    self.finished = true;
                    Ok(None)
                }
            }
        }

        async fn send_message(&mut self, _message: &str) -> color_eyre::Result<()> {
            Ok(())
        }

        fn accepts_input(&self) -> bool {
            self.accepts
        }

        fn session_id(&self) -> Option<&str> {
            self.session_id.as_deref()
        }

        async fn interrupt(&mut self) -> color_eyre::Result<()> {
            Ok(())
        }

        fn is_finished(&self) -> bool {
            self.finished
        }

        async fn wait_for_stderr(&mut self) -> Option<String> {
            None
        }
    }

    fn test_handle(agent: MockAgent, work_dir: &Path) -> AgentHandle {
        let event_log_path = work_dir
            .join(".swarm")
            .join("agents")
            .join("test-worker")
            .join("events.jsonl");
        let (event_tx, _) = broadcast::channel(16);
        AgentHandle {
            worktree_id: "test-worker".to_string(),
            agent: Box::new(agent),
            event_tx,
            logger: EventLogger::new(event_log_path),
        }
    }

    // ── drain_agent_events tests ─────────────────────────

    #[tokio::test]
    async fn drain_with_events_logs_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let (sv_tx, _sv_rx) = mpsc::unbounded_channel();

        let agent = MockAgent::from_events(vec![
            Some(AgentEventWire::TextDelta {
                text: "hello world".into(),
            }),
            Some(AgentEventWire::ToolUse {
                tool: "Bash".into(),
                input: "ls".into(),
            }),
            Some(AgentEventWire::ToolResult {
                output: "file.rs".into(),
                is_error: false,
            }),
            Some(AgentEventWire::SessionResult {
                turns: 3,
                cost_usd: Some(0.05),
                session_id: Some("sess-1".into()),
            }),
        ]);
        let mut handle = test_handle(agent, dir.path());

        let result = drain_agent_events(&mut handle, &sv_tx, dir.path(), None).await;

        // Should complete normally
        assert!(matches!(result, AgentExitReason::Completed(Some(ref id)) if id == "sess-1"));

        // Events should be logged to file
        let events_path = dir.path().join(".swarm/agents/test-worker/events.jsonl");
        assert!(events_path.exists(), "events.jsonl should exist");
        let content = std::fs::read_to_string(&events_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 4, "should have 4 logged events");
        assert!(lines[0].contains("assistant_text"));
        assert!(lines[1].contains("tool_use"));
        assert!(lines[2].contains("tool_result"));
        assert!(lines[3].contains("session_result"));
    }

    #[tokio::test]
    async fn drain_with_zero_events_logs_warning() {
        let dir = tempfile::tempdir().unwrap();
        let (sv_tx, _sv_rx) = mpsc::unbounded_channel();

        // Agent immediately returns None (EOF, zero events)
        let agent = MockAgent::from_events(vec![None]);
        let mut handle = test_handle(agent, dir.path());

        let result = drain_agent_events(&mut handle, &sv_tx, dir.path(), None).await;

        // Should still complete (EOF = completed)
        assert!(matches!(result, AgentExitReason::Completed(None)));

        // Error event should be logged to events.jsonl
        let events_path = dir.path().join(".swarm/agents/test-worker/events.jsonl");
        assert!(
            events_path.exists(),
            "events.jsonl should be created with error"
        );
        let content = std::fs::read_to_string(&events_path).unwrap();
        assert!(content.contains("Agent process exited without producing any events"));
    }

    #[tokio::test]
    async fn drain_with_error_returns_crashed() {
        let dir = tempfile::tempdir().unwrap();
        let (sv_tx, _sv_rx) = mpsc::unbounded_channel();

        let agent = MockAgent::from_results(vec![
            Ok(Some(AgentEventWire::TextDelta {
                text: "starting".into(),
            })),
            Err(color_eyre::eyre::eyre!("connection lost")),
        ]);
        let mut handle = test_handle(agent, dir.path());

        let result = drain_agent_events(&mut handle, &sv_tx, dir.path(), None).await;

        match result {
            AgentExitReason::Crashed(msg) => {
                assert!(msg.contains("connection lost"));
            }
            other => panic!("expected Crashed, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[tokio::test]
    async fn drain_broadcasts_events_to_subscribers() {
        let dir = tempfile::tempdir().unwrap();
        let (sv_tx, _sv_rx) = mpsc::unbounded_channel();

        let agent = MockAgent::from_events(vec![
            Some(AgentEventWire::TextDelta {
                text: "hello".into(),
            }),
            None,
        ]);
        let mut handle = test_handle(agent, dir.path());

        // Subscribe to broadcast
        let mut event_rx = handle.event_tx.subscribe();

        drain_agent_events(&mut handle, &sv_tx, dir.path(), None).await;

        // Should have received the event
        let received = event_rx.try_recv().unwrap();
        assert!(matches!(
            received,
            DaemonResponse::AgentEvent {
                worktree_id: ref id,
                event: AgentEventWire::TextDelta { ref text },
            } if id == "test-worker" && text == "hello"
        ));
    }

    #[tokio::test]
    async fn drain_sends_supervisor_events() {
        let dir = tempfile::tempdir().unwrap();
        let (sv_tx, mut sv_rx) = mpsc::unbounded_channel();

        let agent = MockAgent::from_events(vec![
            Some(AgentEventWire::TextDelta { text: "hi".into() }),
            None,
        ]);
        let mut handle = test_handle(agent, dir.path());

        drain_agent_events(&mut handle, &sv_tx, dir.path(), None).await;

        let event = sv_rx.try_recv().unwrap();
        assert!(matches!(event, SupervisorEvent::AgentEvent { .. }));
    }

    #[tokio::test]
    async fn drain_writes_agent_status_file() {
        let dir = tempfile::tempdir().unwrap();
        let (sv_tx, _sv_rx) = mpsc::unbounded_channel();

        let agent = MockAgent::from_events(vec![None]);
        let mut handle = test_handle(agent, dir.path());

        drain_agent_events(&mut handle, &sv_tx, dir.path(), None).await;

        let status_path = dir.path().join(".swarm/agent-status/test-worker");
        assert!(status_path.exists());
        let status = std::fs::read_to_string(&status_path).unwrap();
        assert_eq!(status, "running");
    }

    // ── log_agent_event tests ────────────────────────────

    #[test]
    fn log_agent_event_text_delta() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let logger = EventLogger::new(path.clone());

        log_agent_event(
            &logger,
            &AgentEventWire::TextDelta {
                text: "hello".into(),
            },
        );

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("assistant_text"));
        assert!(content.contains("hello"));
    }

    #[test]
    fn log_agent_event_tool_use() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let logger = EventLogger::new(path.clone());

        log_agent_event(
            &logger,
            &AgentEventWire::ToolUse {
                tool: "Read".into(),
                input: "main.rs".into(),
            },
        );

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("tool_use"));
        assert!(content.contains("Read"));
        assert!(content.contains("main.rs"));
    }

    #[test]
    fn log_agent_event_tool_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let logger = EventLogger::new(path.clone());

        log_agent_event(
            &logger,
            &AgentEventWire::ToolResult {
                output: "file contents".into(),
                is_error: true,
            },
        );

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("tool_result"));
        assert!(content.contains("true")); // is_error
    }

    #[test]
    fn log_agent_event_session_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let logger = EventLogger::new(path.clone());

        log_agent_event(
            &logger,
            &AgentEventWire::SessionResult {
                turns: 5,
                cost_usd: Some(0.12),
                session_id: Some("sess-abc".into()),
            },
        );

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("session_result"));
        assert!(content.contains("sess-abc"));
    }

    #[test]
    fn log_agent_event_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let logger = EventLogger::new(path.clone());

        log_agent_event(
            &logger,
            &AgentEventWire::Error {
                message: "rate limited".into(),
            },
        );

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("error"));
        assert!(content.contains("rate limited"));
    }

    #[test]
    fn log_agent_event_skips_thinking_and_turn_complete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let logger = EventLogger::new(path.clone());

        log_agent_event(
            &logger,
            &AgentEventWire::ThinkingDelta { text: "hmm".into() },
        );
        log_agent_event(&logger, &AgentEventWire::TurnComplete);

        // File should not exist (no events worth logging)
        assert!(!path.exists());
    }

    // ── agent_event_loop tests ───────────────────────────

    #[tokio::test]
    async fn event_loop_normal_completion() {
        let dir = tempfile::tempdir().unwrap();
        let (sv_tx, _sv_rx) = mpsc::unbounded_channel();

        let mut agent = MockAgent::from_events(vec![
            Some(AgentEventWire::TextDelta {
                text: "done".into(),
            }),
            Some(AgentEventWire::SessionResult {
                turns: 1,
                cost_usd: None,
                session_id: Some("s1".into()),
            }),
        ]);
        // After SessionResult, agent is finished
        agent.finished = false;
        let mut handle = test_handle(agent, dir.path());

        let mut restart_count = 0u32;
        let (phase, session_id) = agent_event_loop(
            &mut handle,
            EventLoopOpts {
                supervisor_tx: &sv_tx,
                work_dir: dir.path(),
                restart_count: &mut restart_count,
                kind: AgentKind::Claude,
                prompt: "test",
                worktree_path: dir.path(),
                dangerously_skip_permissions: true,
            },
        )
        .await;

        // Claude agents don't accept input when finished → Completed
        assert_eq!(phase, WorkerPhase::Completed);
        assert_eq!(session_id, Some("s1".into()));
        assert_eq!(restart_count, 0);
    }

    #[tokio::test]
    async fn event_loop_waiting_for_interactive_agent() {
        let dir = tempfile::tempdir().unwrap();
        let (sv_tx, mut sv_rx) = mpsc::unbounded_channel();

        let mut agent = MockAgent::from_events(vec![Some(AgentEventWire::SessionResult {
            turns: 1,
            cost_usd: None,
            session_id: Some("s1".into()),
        })]);
        agent.accepts = true; // Interactive agent
        let mut handle = test_handle(agent, dir.path());

        let mut restart_count = 0u32;
        let (phase, _) = agent_event_loop(
            &mut handle,
            EventLoopOpts {
                supervisor_tx: &sv_tx,
                work_dir: dir.path(),
                restart_count: &mut restart_count,
                kind: AgentKind::Claude,
                prompt: "test",
                worktree_path: dir.path(),
                dangerously_skip_permissions: true,
            },
        )
        .await;

        assert_eq!(phase, WorkerPhase::Waiting);

        // Should have emitted PhaseChanged to Waiting
        let mut found_waiting = false;
        while let Ok(event) = sv_rx.try_recv() {
            if matches!(
                event,
                SupervisorEvent::PhaseChanged {
                    phase: WorkerPhase::Waiting,
                    ..
                }
            ) {
                found_waiting = true;
            }
        }
        assert!(found_waiting, "should emit PhaseChanged::Waiting");
    }

    // ── stall/timeout detection tests ────────────────────

    /// Mock agent that introduces a delay before returning events.
    /// Used for testing timeout behavior.
    struct SlowMockAgent {
        delay: Duration,
        events: VecDeque<Option<AgentEventWire>>,
        accepts: bool,
        finished: bool,
    }

    impl SlowMockAgent {
        fn new(delay: Duration, events: Vec<Option<AgentEventWire>>) -> Self {
            Self {
                delay,
                events: events.into(),
                accepts: true,
                finished: false,
            }
        }
    }

    #[async_trait]
    impl ManagedAgent for SlowMockAgent {
        fn kind(&self) -> AgentKind {
            AgentKind::Claude
        }

        async fn next_event(&mut self) -> color_eyre::Result<Option<AgentEventWire>> {
            tokio::time::sleep(self.delay).await;
            match self.events.pop_front() {
                Some(event) => {
                    if event.is_none() {
                        self.finished = true;
                    }
                    Ok(event)
                }
                None => {
                    self.finished = true;
                    Ok(None)
                }
            }
        }

        async fn send_message(&mut self, _message: &str) -> color_eyre::Result<()> {
            Ok(())
        }

        fn accepts_input(&self) -> bool {
            self.accepts
        }

        fn session_id(&self) -> Option<&str> {
            None
        }

        async fn interrupt(&mut self) -> color_eyre::Result<()> {
            Ok(())
        }

        fn is_finished(&self) -> bool {
            self.finished
        }

        async fn wait_for_stderr(&mut self) -> Option<String> {
            None
        }
    }

    fn test_handle_dyn(agent: Box<dyn ManagedAgent>, work_dir: &Path) -> AgentHandle {
        let event_log_path = work_dir
            .join(".swarm")
            .join("agents")
            .join("test-worker")
            .join("events.jsonl");
        let (event_tx, _) = broadcast::channel(16);
        AgentHandle {
            worktree_id: "test-worker".to_string(),
            agent,
            event_tx,
            logger: EventLogger::new(event_log_path),
        }
    }

    // All stall/timeout tests use `start_paused = true` so tokio auto-advances
    // the timer. This makes tests deterministic regardless of CI runner speed.

    #[tokio::test(start_paused = true)]
    async fn drain_stall_detected_when_first_event_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let (sv_tx, _sv_rx) = mpsc::unbounded_channel();

        // Agent takes 10s to produce first event, but timeout is 1s.
        let agent = SlowMockAgent::new(
            Duration::from_secs(10),
            vec![Some(AgentEventWire::TextDelta {
                text: "late".into(),
            })],
        );
        let mut handle = test_handle_dyn(Box::new(agent), dir.path());

        let result = drain_agent_events(
            &mut handle,
            &sv_tx,
            dir.path(),
            Some(Duration::from_secs(1)),
        )
        .await;

        assert!(
            matches!(result, AgentExitReason::Stalled),
            "should detect stall"
        );

        // Stall error should be logged to events.jsonl
        let events_path = dir.path().join(".swarm/agents/test-worker/events.jsonl");
        assert!(events_path.exists());
        let content = std::fs::read_to_string(&events_path).unwrap();
        assert!(content.contains("stalled"));
    }

    #[tokio::test(start_paused = true)]
    async fn drain_normal_fast_response_with_first_event_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let (sv_tx, _sv_rx) = mpsc::unbounded_channel();

        // Agent responds in 1s, well within the 60s timeout.
        let agent = SlowMockAgent::new(
            Duration::from_secs(1),
            vec![
                Some(AgentEventWire::TextDelta {
                    text: "fast".into(),
                }),
                Some(AgentEventWire::SessionResult {
                    turns: 1,
                    cost_usd: None,
                    session_id: Some("s-fast".into()),
                }),
            ],
        );
        let mut handle = test_handle_dyn(Box::new(agent), dir.path());

        let result = drain_agent_events(
            &mut handle,
            &sv_tx,
            dir.path(),
            Some(Duration::from_secs(60)),
        )
        .await;

        assert!(
            matches!(result, AgentExitReason::Completed(Some(ref id)) if id == "s-fast"),
            "fast agent should complete normally"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn event_loop_with_timeouts_total_timeout_fires() {
        let dir = tempfile::tempdir().unwrap();
        let (sv_tx, mut sv_rx) = mpsc::unbounded_channel();

        // Agent produces events slowly (10s each) but never completes.
        // Total timeout (60s) fires before all 100 events are drained.
        let mut events: Vec<Option<AgentEventWire>> = (0..100)
            .map(|i| {
                Some(AgentEventWire::TextDelta {
                    text: format!("event-{}", i),
                })
            })
            .collect();
        events.push(None); // EOF at end (but total timeout should fire first)

        let agent = SlowMockAgent::new(Duration::from_secs(10), events);
        let mut handle = test_handle_dyn(Box::new(agent), dir.path());

        let mut restart_count = 0u32;
        let (phase, _session_id) = agent_event_loop_with_timeouts(
            &mut handle,
            EventLoopOpts {
                supervisor_tx: &sv_tx,
                work_dir: dir.path(),
                restart_count: &mut restart_count,
                kind: AgentKind::Claude,
                prompt: "test",
                worktree_path: dir.path(),
                dangerously_skip_permissions: true,
            },
            Some(Duration::from_secs(30)), // generous first-event timeout
            Some(Duration::from_secs(60)), // total timeout
        )
        .await;

        assert_eq!(
            phase,
            WorkerPhase::Waiting,
            "stalled agent should return to Waiting"
        );

        // Should have emitted PhaseChanged to Waiting
        let mut found_waiting = false;
        while let Ok(event) = sv_rx.try_recv() {
            if matches!(
                event,
                SupervisorEvent::PhaseChanged {
                    phase: WorkerPhase::Waiting,
                    ..
                }
            ) {
                found_waiting = true;
            }
        }
        assert!(
            found_waiting,
            "should emit PhaseChanged::Waiting on total timeout"
        );

        // Stall should be logged
        let events_path = dir.path().join(".swarm/agents/test-worker/events.jsonl");
        assert!(events_path.exists());
        let content = std::fs::read_to_string(&events_path).unwrap();
        assert!(content.contains("stalled") || content.contains("timeout"));
    }

    #[tokio::test(start_paused = true)]
    async fn event_loop_with_timeouts_first_event_stall_returns_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let (sv_tx, mut sv_rx) = mpsc::unbounded_channel();

        // Agent never responds (very long delay).
        let agent = SlowMockAgent::new(Duration::from_secs(3600), vec![]);
        let mut handle = test_handle_dyn(Box::new(agent), dir.path());

        let mut restart_count = 0u32;
        let (phase, _) = agent_event_loop_with_timeouts(
            &mut handle,
            EventLoopOpts {
                supervisor_tx: &sv_tx,
                work_dir: dir.path(),
                restart_count: &mut restart_count,
                kind: AgentKind::Claude,
                prompt: "test",
                worktree_path: dir.path(),
                dangerously_skip_permissions: true,
            },
            Some(Duration::from_secs(5)),   // first-event timeout: 5s
            Some(Duration::from_secs(600)), // total timeout: 10min (won't fire)
        )
        .await;

        // Should return Waiting, not Failed
        assert_eq!(
            phase,
            WorkerPhase::Waiting,
            "stalled agent should return to Waiting, not Failed"
        );
        assert_eq!(restart_count, 0, "stall should not increment restart count");

        // Should emit PhaseChanged::Waiting
        let mut found_waiting = false;
        while let Ok(event) = sv_rx.try_recv() {
            if matches!(
                event,
                SupervisorEvent::PhaseChanged {
                    phase: WorkerPhase::Waiting,
                    ..
                }
            ) {
                found_waiting = true;
            }
        }
        assert!(found_waiting);
    }
}
