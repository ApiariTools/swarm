use super::managed_agent::{self, ManagedAgent, SpawnOptions};
use super::protocol::{AgentEventWire, DaemonResponse};
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
    let mut event_count: u64 = 0;

    swarm_log!(
        "[daemon] Agent {} — drain_agent_events: waiting for first event...",
        handle.worktree_id
    );

    loop {
        match handle.agent.next_event().await {
            Ok(Some(event)) => {
                event_count += 1;

                if event_count <= 3 || event_count % 50 == 0 {
                    swarm_log!(
                        "[daemon] Agent {} — event #{}: {:?}",
                        handle.worktree_id,
                        event_count,
                        std::mem::discriminant(&event)
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
                    swarm_log!(
                        "[daemon] Agent {} completed with SessionResult ({} events)",
                        handle.worktree_id,
                        event_count
                    );
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
                    swarm_log!(
                        "[daemon] WARNING: Agent {} exited with ZERO events. stderr: {}",
                        handle.worktree_id,
                        stderr_msg
                    );
                    handle.logger.log_error(
                        &format!("Agent process exited without producing any events. stderr: {}", stderr_msg),
                    );
                } else {
                    swarm_log!(
                        "[daemon] Agent {} EOF after {} events (no SessionResult)",
                        handle.worktree_id,
                        event_count
                    );
                }
                let session_id = handle.agent.session_id().map(String::from);
                return AgentExitReason::Completed(session_id);
            }
            Err(e) => {
                swarm_log!(
                    "[daemon] Agent {} errored after {} events: {}",
                    handle.worktree_id,
                    event_count,
                    e
                );
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

        fn from_results(
            results: Vec<Result<Option<AgentEventWire>, color_eyre::Report>>,
        ) -> Self {
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

        let result = drain_agent_events(&mut handle, &sv_tx, dir.path()).await;

        // Should complete normally
        assert!(matches!(result, AgentExitReason::Completed(Some(ref id)) if id == "sess-1"));

        // Events should be logged to file
        let events_path = dir
            .path()
            .join(".swarm/agents/test-worker/events.jsonl");
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

        let result = drain_agent_events(&mut handle, &sv_tx, dir.path()).await;

        // Should still complete (EOF = completed)
        assert!(matches!(result, AgentExitReason::Completed(None)));

        // Error event should be logged to events.jsonl
        let events_path = dir
            .path()
            .join(".swarm/agents/test-worker/events.jsonl");
        assert!(events_path.exists(), "events.jsonl should be created with error");
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

        let result = drain_agent_events(&mut handle, &sv_tx, dir.path()).await;

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

        drain_agent_events(&mut handle, &sv_tx, dir.path()).await;

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
            Some(AgentEventWire::TextDelta {
                text: "hi".into(),
            }),
            None,
        ]);
        let mut handle = test_handle(agent, dir.path());

        drain_agent_events(&mut handle, &sv_tx, dir.path()).await;

        let event = sv_rx.try_recv().unwrap();
        assert!(matches!(event, SupervisorEvent::AgentEvent { .. }));
    }

    #[tokio::test]
    async fn drain_writes_agent_status_file() {
        let dir = tempfile::tempdir().unwrap();
        let (sv_tx, _sv_rx) = mpsc::unbounded_channel();

        let agent = MockAgent::from_events(vec![None]);
        let mut handle = test_handle(agent, dir.path());

        drain_agent_events(&mut handle, &sv_tx, dir.path()).await;

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
            &AgentEventWire::ThinkingDelta {
                text: "hmm".into(),
            },
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

        let mut agent = MockAgent::from_events(vec![
            Some(AgentEventWire::SessionResult {
                turns: 1,
                cost_usd: None,
                session_id: Some("s1".into()),
            }),
        ]);
        agent.accepts = true; // Interactive agent
        let mut handle = test_handle(agent, dir.path());

        let mut restart_count = 0u32;
        let (phase, _) = agent_event_loop(
            &mut handle,
            EventLoopOpts {
                supervisor_tx: &sv_tx,
                work_dir: dir.path(),
                restart_count: &mut restart_count,
                kind: AgentKind::ClaudeTui,
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
}
