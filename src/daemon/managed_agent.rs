use super::protocol::AgentEventWire;
use crate::core::agent::AgentKind;
use async_trait::async_trait;
use color_eyre::Result;
use std::path::PathBuf;

/// Unified interface for managing agent processes (Claude, Codex, or Gemini).
///
/// Each implementation wraps the SDK's session/execution handle and translates
/// raw SDK events into [`AgentEventWire`] for the daemon to broadcast.
#[async_trait]
#[allow(dead_code)]
pub trait ManagedAgent: Send {
    /// What kind of agent this is.
    fn kind(&self) -> AgentKind;

    /// Get the next event from the agent. Returns `None` when the agent has
    /// finished (session complete or process exited).
    async fn next_event(&mut self) -> Result<Option<AgentEventWire>>;

    /// Send a follow-up message to the agent. For Claude, this resumes the
    /// session. For Codex, this starts a new execution with session resume.
    async fn send_message(&mut self, message: &str) -> Result<()>;

    /// Whether the agent currently accepts input (i.e. is in a waiting state).
    fn accepts_input(&self) -> bool;

    /// The session ID for resume, if available.
    fn session_id(&self) -> Option<&str>;

    /// Send an interrupt signal (SIGINT) to the agent process.
    async fn interrupt(&mut self) -> Result<()>;

    /// Returns `true` if the agent has finished.
    fn is_finished(&self) -> bool;

    /// Wait for the agent process to exit and return captured stderr (if any).
    /// Used for diagnostics when the agent exits unexpectedly.
    async fn wait_for_stderr(&mut self) -> Option<String>;
}

/// Options for spawning a managed agent.
pub struct SpawnOptions {
    pub kind: AgentKind,
    pub prompt: String,
    pub working_dir: PathBuf,
    pub dangerously_skip_permissions: bool,
    pub resume_session_id: Option<String>,
    pub max_turns: Option<u64>,
}

/// Spawn a new ManagedAgent based on the agent kind.
pub async fn spawn_managed_agent(opts: SpawnOptions) -> Result<Box<dyn ManagedAgent>> {
    match opts.kind {
        AgentKind::Claude => {
            let agent = ClaudeManagedAgent::spawn(opts).await?;
            Ok(Box::new(agent))
        }
        AgentKind::Codex => {
            let agent = CodexManagedAgent::spawn(opts).await?;
            Ok(Box::new(agent))
        }
        AgentKind::Gemini => {
            let agent = GeminiManagedAgent::spawn(opts).await?;
            Ok(Box::new(agent))
        }
    }
}

// ── Claude Managed Agent ─────────────────────────────────

/// Agent state machine for Claude SDK sessions.
enum ClaudeState {
    /// Actively draining events from a session.
    Running(Box<apiari_claude_sdk::Session>),
    /// Session completed, waiting for follow-up message.
    Waiting,
    /// Session finished, no more events.
    Finished,
}

/// Wraps a Claude SDK session for daemon management.
pub struct ClaudeManagedAgent {
    state: ClaudeState,
    session_id: Option<String>,
    working_dir: PathBuf,
    dangerously_skip: bool,
    max_turns: Option<u64>,
}

impl ClaudeManagedAgent {
    async fn spawn(opts: SpawnOptions) -> Result<Self> {
        let session_opts = apiari_claude_sdk::SessionOptions {
            resume: opts.resume_session_id.clone(),
            dangerously_skip_permissions: opts.dangerously_skip_permissions,
            include_partial_messages: true,
            working_dir: Some(opts.working_dir.clone()),
            max_turns: opts.max_turns,
            ..Default::default()
        };

        let client = apiari_claude_sdk::ClaudeClient::new();
        let mut session = client.spawn(session_opts).await?;

        // If this is a fresh session (not resuming), send the initial prompt
        if opts.resume_session_id.is_none() {
            session.send_message(&opts.prompt).await?;
        }

        Ok(Self {
            state: if opts.resume_session_id.is_some() {
                // Resuming: jump straight to waiting for follow-up
                ClaudeState::Waiting
            } else {
                ClaudeState::Running(Box::new(session))
            },
            session_id: opts.resume_session_id,
            working_dir: opts.working_dir,
            dangerously_skip: opts.dangerously_skip_permissions,
            max_turns: opts.max_turns,
        })
    }
}

#[async_trait]
impl ManagedAgent for ClaudeManagedAgent {
    fn kind(&self) -> AgentKind {
        AgentKind::Claude
    }

    async fn next_event(&mut self) -> Result<Option<AgentEventWire>> {
        loop {
            let next = match &mut self.state {
                ClaudeState::Running(session) => session.next_event().await,
                ClaudeState::Waiting | ClaudeState::Finished => return Ok(None),
            };

            match next {
                Ok(Some(event)) => {
                    // Capture session_id from Result.
                    if let apiari_claude_sdk::Event::Result(ref result) = event {
                        self.session_id = Some(result.session_id.clone());
                        // Transition to Waiting after Result.
                        self.state = ClaudeState::Waiting;
                    }

                    // Skip non-user-facing events (system/user/rate-limit) without
                    // signaling EOF to the supervisor.
                    if let Some(wire) = translate_claude_event(&event) {
                        return Ok(Some(wire));
                    }
                }
                Ok(None) => {
                    self.state = ClaudeState::Finished;
                    return Ok(None);
                }
                Err(e) => {
                    self.state = ClaudeState::Finished;
                    return Err(e.into());
                }
            }
        }
    }

    async fn send_message(&mut self, message: &str) -> Result<()> {
        if !self.accepts_input() {
            return Err(color_eyre::eyre::eyre!("agent not accepting input"));
        }

        // Resume the session with the saved session_id
        let resume_opts = apiari_claude_sdk::SessionOptions {
            resume: self.session_id.clone(),
            dangerously_skip_permissions: self.dangerously_skip,
            include_partial_messages: true,
            working_dir: Some(self.working_dir.clone()),
            max_turns: self.max_turns,
            ..Default::default()
        };

        let client = apiari_claude_sdk::ClaudeClient::new();
        let mut session = client.spawn(resume_opts).await?;
        session.send_message(message).await?;
        self.state = ClaudeState::Running(Box::new(session));
        Ok(())
    }

    fn accepts_input(&self) -> bool {
        matches!(self.state, ClaudeState::Waiting)
    }

    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    async fn interrupt(&mut self) -> Result<()> {
        if let ClaudeState::Running(session) = &mut self.state {
            session.interrupt().await?;
        }
        Ok(())
    }

    fn is_finished(&self) -> bool {
        matches!(self.state, ClaudeState::Finished)
    }

    async fn wait_for_stderr(&mut self) -> Option<String> {
        if let ClaudeState::Running(ref mut session) = self.state {
            session.wait_for_stderr().await.ok().flatten()
        } else {
            None
        }
    }
}

/// Translate a Claude SDK event into a wire-format AgentEventWire.
fn translate_claude_event(event: &apiari_claude_sdk::Event) -> Option<AgentEventWire> {
    match event {
        apiari_claude_sdk::Event::Stream { assembled, .. } => {
            use apiari_claude_sdk::streaming::AssembledEvent;
            use apiari_claude_sdk::types::ContentBlock;

            // Return the first meaningful event from assembled events
            for asm in assembled {
                match asm {
                    AssembledEvent::TextDelta { text, .. } => {
                        return Some(AgentEventWire::TextDelta { text: text.clone() });
                    }
                    AssembledEvent::ThinkingDelta { .. } => {
                        return Some(AgentEventWire::ThinkingDelta {
                            text: String::new(),
                        });
                    }
                    AssembledEvent::ContentBlockComplete { block, .. } => match block {
                        ContentBlock::ToolUse { name, input, .. } => {
                            let input_str =
                                serde_json::to_string(input).unwrap_or_else(|_| input.to_string());
                            return Some(AgentEventWire::ToolUse {
                                tool: name.clone(),
                                input: input_str,
                            });
                        }
                        ContentBlock::ToolResult {
                            content, is_error, ..
                        } => {
                            let output = content
                                .as_ref()
                                .map(|v| {
                                    v.as_str()
                                        .map(String::from)
                                        .unwrap_or_else(|| v.to_string())
                                })
                                .unwrap_or_default();
                            return Some(AgentEventWire::ToolResult {
                                output,
                                is_error: is_error.unwrap_or(false),
                            });
                        }
                        ContentBlock::Text { .. } => {
                            // Text already delivered via TextDelta events;
                            // ContentBlockComplete contains the full accumulated
                            // text which would duplicate it.
                        }
                        _ => {}
                    },
                    AssembledEvent::MessageComplete { .. } => {
                        return Some(AgentEventWire::TurnComplete);
                    }
                    AssembledEvent::MessageStart { .. } => {}
                }
            }
            None
        }
        apiari_claude_sdk::Event::Assistant { .. } => {
            // With include_partial_messages=true (always set), all content
            // (text, tool_use, tool_result, turn boundaries) has already been
            // delivered via Stream events. Skip to avoid duplication.
            None
        }
        apiari_claude_sdk::Event::Result(result) => Some(AgentEventWire::SessionResult {
            turns: result.num_turns,
            cost_usd: result.total_cost_usd,
            session_id: Some(result.session_id.clone()),
        }),
        apiari_claude_sdk::Event::System(_)
        | apiari_claude_sdk::Event::User(_)
        | apiari_claude_sdk::Event::RateLimit(_) => None,
    }
}

// ── Codex Managed Agent ──────────────────────────────────

/// Agent state machine for Codex SDK executions.
enum CodexState {
    /// Actively draining events from an execution.
    Running(Box<apiari_codex_sdk::Execution>),
    /// Execution completed, waiting for follow-up.
    Waiting,
    /// Execution finished permanently.
    Finished,
}

/// Wraps a Codex SDK execution for daemon management.
pub struct CodexManagedAgent {
    state: CodexState,
    thread_id: Option<String>,
    working_dir: PathBuf,
}

impl CodexManagedAgent {
    async fn spawn(opts: SpawnOptions) -> Result<Self> {
        let client = apiari_codex_sdk::CodexClient::new();

        let execution = if let Some(ref session_id) = opts.resume_session_id {
            client
                .exec_resume(
                    &opts.prompt,
                    apiari_codex_sdk::ResumeOptions {
                        session_id: Some(session_id.clone()),
                        full_auto: true,
                        working_dir: Some(opts.working_dir.clone()),
                        ..Default::default()
                    },
                )
                .await?
        } else {
            client
                .exec(
                    &opts.prompt,
                    apiari_codex_sdk::ExecOptions {
                        full_auto: true,
                        working_dir: Some(opts.working_dir.clone()),
                        ..Default::default()
                    },
                )
                .await?
        };

        Ok(Self {
            state: CodexState::Running(Box::new(execution)),
            thread_id: opts.resume_session_id,
            working_dir: opts.working_dir,
        })
    }
}

#[async_trait]
impl ManagedAgent for CodexManagedAgent {
    fn kind(&self) -> AgentKind {
        AgentKind::Codex
    }

    async fn next_event(&mut self) -> Result<Option<AgentEventWire>> {
        loop {
            let (next, execution_finished) = match &mut self.state {
                CodexState::Running(exec) => (exec.next_event().await, exec.is_finished()),
                CodexState::Waiting | CodexState::Finished => return Ok(None),
            };

            match next {
                Ok(Some(event)) => {
                    // Track thread_id.
                    if let apiari_codex_sdk::Event::ThreadStarted { ref thread_id } = event {
                        self.thread_id = Some(thread_id.clone());
                    }

                    // Check if execution is done.
                    if execution_finished {
                        self.state = CodexState::Waiting;
                    }

                    // Skip non-user-facing events without signaling EOF.
                    if let Some(wire) = translate_codex_event(&event) {
                        return Ok(Some(wire));
                    }
                }
                Ok(None) => {
                    // EOF — execution finished.
                    self.state = CodexState::Waiting;
                    return Ok(None);
                }
                Err(e) => {
                    self.state = CodexState::Finished;
                    return Err(e.into());
                }
            }
        }
    }

    async fn send_message(&mut self, message: &str) -> Result<()> {
        if !self.accepts_input() {
            return Err(color_eyre::eyre::eyre!("codex agent not accepting input"));
        }

        let client = apiari_codex_sdk::CodexClient::new();
        let execution = client
            .exec_resume(
                message,
                apiari_codex_sdk::ResumeOptions {
                    session_id: self.thread_id.clone(),
                    full_auto: true,
                    working_dir: Some(self.working_dir.clone()),
                    ..Default::default()
                },
            )
            .await?;

        self.state = CodexState::Running(Box::new(execution));
        Ok(())
    }

    fn accepts_input(&self) -> bool {
        matches!(self.state, CodexState::Waiting)
    }

    fn session_id(&self) -> Option<&str> {
        self.thread_id.as_deref()
    }

    async fn interrupt(&mut self) -> Result<()> {
        if let CodexState::Running(ref exec) = self.state {
            exec.interrupt()?;
        }
        Ok(())
    }

    fn is_finished(&self) -> bool {
        matches!(self.state, CodexState::Finished)
    }

    async fn wait_for_stderr(&mut self) -> Option<String> {
        // Codex SDK doesn't expose stderr currently
        None
    }
}

/// Translate a Codex SDK event into a wire-format AgentEventWire.
fn translate_codex_event(event: &apiari_codex_sdk::Event) -> Option<AgentEventWire> {
    use apiari_codex_sdk::{Event, Item};

    match event {
        Event::ItemCompleted {
            item: Item::AgentMessage { text, .. },
        }
        | Event::ItemUpdated {
            item: Item::AgentMessage { text, .. },
        } => text
            .as_ref()
            .map(|t| AgentEventWire::TextDelta { text: t.clone() }),
        Event::ItemCompleted {
            item: Item::Reasoning { text, .. },
        }
        | Event::ItemUpdated {
            item: Item::Reasoning { text, .. },
        } => text
            .as_ref()
            .map(|t| AgentEventWire::ThinkingDelta { text: t.clone() }),
        Event::ItemCompleted {
            item:
                Item::CommandExecution {
                    aggregated_output,
                    exit_code,
                    ..
                },
        } => {
            let output = aggregated_output.clone().unwrap_or_default();
            let is_error = exit_code.is_some_and(|c| c != 0);
            // Emit both ToolUse and ToolResult for command executions
            // Return ToolResult as the primary event; ToolUse was already emitted at ItemStarted
            Some(AgentEventWire::ToolResult { output, is_error })
        }
        Event::ItemStarted {
            item: Item::CommandExecution { command, .. },
        } => Some(AgentEventWire::ToolUse {
            tool: "Bash".into(),
            input: command.clone().unwrap_or_default(),
        }),
        Event::ItemCompleted {
            item: Item::FileChange { changes, .. },
        } => {
            let files: Vec<String> = changes.iter().filter_map(|c| c.file_path.clone()).collect();
            Some(AgentEventWire::ToolUse {
                tool: "FileChange".into(),
                input: files.join(", "),
            })
        }
        Event::TurnCompleted { usage } => {
            let turns = usage.as_ref().map(|u| u.total_tokens).unwrap_or(0);
            Some(AgentEventWire::SessionResult {
                turns,
                cost_usd: None,
                session_id: None,
            })
        }
        Event::TurnFailed { error, .. } => {
            let msg = error
                .as_ref()
                .and_then(|e| e.message.clone())
                .unwrap_or_else(|| "turn failed".into());
            Some(AgentEventWire::Error { message: msg })
        }
        Event::Error { message, .. } => Some(AgentEventWire::Error {
            message: message.clone().unwrap_or_else(|| "unknown error".into()),
        }),
        _ => None,
    }
}

// ── Gemini Managed Agent ─────────────────────────────────

/// Agent state machine for Gemini SDK executions.
enum GeminiState {
    /// Actively draining events from an execution.
    Running(Box<apiari_gemini_sdk::Execution>),
    /// Execution completed, waiting for follow-up.
    Waiting,
    /// Execution finished permanently.
    Finished,
}

/// Wraps a Gemini SDK execution for daemon management.
pub struct GeminiManagedAgent {
    state: GeminiState,
    thread_id: Option<String>,
    working_dir: PathBuf,
}

impl GeminiManagedAgent {
    async fn spawn(opts: SpawnOptions) -> Result<Self> {
        let client = apiari_gemini_sdk::GeminiClient::new();

        let execution = if let Some(ref session_id) = opts.resume_session_id {
            client
                .exec_resume(
                    &opts.prompt,
                    apiari_gemini_sdk::SessionOptions {
                        session_id: Some(session_id.clone()),
                        working_dir: Some(opts.working_dir.clone()),
                        ..Default::default()
                    },
                )
                .await?
        } else {
            client
                .exec(
                    &opts.prompt,
                    apiari_gemini_sdk::GeminiOptions {
                        working_dir: Some(opts.working_dir.clone()),
                        ..Default::default()
                    },
                )
                .await?
        };

        Ok(Self {
            state: GeminiState::Running(Box::new(execution)),
            thread_id: opts.resume_session_id,
            working_dir: opts.working_dir,
        })
    }
}

#[async_trait]
impl ManagedAgent for GeminiManagedAgent {
    fn kind(&self) -> AgentKind {
        AgentKind::Gemini
    }

    async fn next_event(&mut self) -> Result<Option<AgentEventWire>> {
        loop {
            let (next, execution_finished) = match &mut self.state {
                GeminiState::Running(exec) => (exec.next_event().await, exec.is_finished()),
                GeminiState::Waiting | GeminiState::Finished => return Ok(None),
            };

            match next {
                Ok(Some(event)) => {
                    // Track thread_id.
                    if let apiari_gemini_sdk::Event::ThreadStarted { ref thread_id } = event {
                        self.thread_id = Some(thread_id.clone());
                    }

                    // Check if execution is done.
                    if execution_finished {
                        self.state = GeminiState::Waiting;
                    }

                    // Skip non-user-facing events without signaling EOF.
                    if let Some(wire) = translate_gemini_event(&event) {
                        return Ok(Some(wire));
                    }
                }
                Ok(None) => {
                    // EOF — execution finished. Emit SessionResult with thread_id
                    // so the daemon can resume the session later.
                    self.state = GeminiState::Waiting;
                    return Ok(Some(AgentEventWire::SessionResult {
                        turns: 0,
                        cost_usd: None,
                        session_id: self.thread_id.clone(),
                    }));
                }
                Err(e) => {
                    self.state = GeminiState::Finished;
                    return Err(e.into());
                }
            }
        }
    }

    async fn send_message(&mut self, message: &str) -> Result<()> {
        if !self.accepts_input() {
            return Err(color_eyre::eyre::eyre!("gemini agent not accepting input"));
        }

        let client = apiari_gemini_sdk::GeminiClient::new();
        let execution = client
            .exec_resume(
                message,
                apiari_gemini_sdk::SessionOptions {
                    session_id: self.thread_id.clone(),
                    working_dir: Some(self.working_dir.clone()),
                    ..Default::default()
                },
            )
            .await?;

        self.state = GeminiState::Running(Box::new(execution));
        Ok(())
    }

    fn accepts_input(&self) -> bool {
        matches!(self.state, GeminiState::Waiting)
    }

    fn session_id(&self) -> Option<&str> {
        self.thread_id.as_deref()
    }

    async fn interrupt(&mut self) -> Result<()> {
        if let GeminiState::Running(ref exec) = self.state {
            exec.interrupt()?;
        }
        Ok(())
    }

    fn is_finished(&self) -> bool {
        matches!(self.state, GeminiState::Finished)
    }

    async fn wait_for_stderr(&mut self) -> Option<String> {
        // Gemini SDK doesn't expose stderr currently
        None
    }
}

/// Translate a Gemini SDK event into a wire-format AgentEventWire.
fn translate_gemini_event(event: &apiari_gemini_sdk::Event) -> Option<AgentEventWire> {
    use apiari_gemini_sdk::{Event, Item};

    match event {
        Event::ItemCompleted {
            item: Item::AgentMessage { text, .. },
        }
        | Event::ItemUpdated {
            item: Item::AgentMessage { text, .. },
        } => text
            .as_ref()
            .map(|t| AgentEventWire::TextDelta { text: t.clone() }),
        Event::ItemCompleted {
            item: Item::Reasoning { text, .. },
        }
        | Event::ItemUpdated {
            item: Item::Reasoning { text, .. },
        } => text
            .as_ref()
            .map(|t| AgentEventWire::ThinkingDelta { text: t.clone() }),
        Event::ItemCompleted {
            item:
                Item::CommandExecution {
                    aggregated_output,
                    exit_code,
                    ..
                },
        } => {
            let output = aggregated_output.clone().unwrap_or_default();
            let is_error = exit_code.is_some_and(|c| c != 0);
            Some(AgentEventWire::ToolResult { output, is_error })
        }
        Event::ItemStarted {
            item: Item::CommandExecution { command, .. },
        } => Some(AgentEventWire::ToolUse {
            tool: "Bash".into(),
            input: command.clone().unwrap_or_default(),
        }),
        Event::ItemCompleted {
            item: Item::ToolCall { tool, .. },
        } => Some(AgentEventWire::ToolUse {
            tool: tool.clone().unwrap_or_default(),
            input: String::new(),
        }),
        Event::ItemCompleted {
            item: Item::FileChange { changes, .. },
        } => {
            let files: Vec<String> = changes.iter().filter_map(|c| c.file_path.clone()).collect();
            Some(AgentEventWire::ToolUse {
                tool: "FileChange".into(),
                input: files.join(", "),
            })
        }
        Event::TurnCompleted { .. } => {
            // TurnCompleted just signals end-of-turn, not end-of-session.
            // SessionResult is emitted at EOF with the tracked thread_id.
            None
        }
        Event::TurnFailed { error, .. } => {
            let msg = error
                .as_ref()
                .and_then(|e| e.message.clone())
                .unwrap_or_else(|| "turn failed".into());
            Some(AgentEventWire::Error { message: msg })
        }
        Event::Error { message, .. } => Some(AgentEventWire::Error {
            message: message.clone().unwrap_or_else(|| "unknown error".into()),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_codex_agent_message() {
        let event = apiari_codex_sdk::Event::ItemCompleted {
            item: apiari_codex_sdk::Item::AgentMessage {
                id: Some("m1".into()),
                text: Some("hello world".into()),
            },
        };
        let wire = translate_codex_event(&event);
        match wire {
            Some(AgentEventWire::TextDelta { text }) => assert_eq!(text, "hello world"),
            other => panic!("expected TextDelta, got {other:?}"),
        }
    }

    #[test]
    fn translate_codex_command_started() {
        let event = apiari_codex_sdk::Event::ItemStarted {
            item: apiari_codex_sdk::Item::CommandExecution {
                id: Some("c1".into()),
                command: Some("ls -la".into()),
                aggregated_output: None,
                exit_code: None,
                status: None,
            },
        };
        let wire = translate_codex_event(&event);
        match wire {
            Some(AgentEventWire::ToolUse { tool, input }) => {
                assert_eq!(tool, "Bash");
                assert_eq!(input, "ls -la");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn translate_codex_command_completed() {
        let event = apiari_codex_sdk::Event::ItemCompleted {
            item: apiari_codex_sdk::Item::CommandExecution {
                id: Some("c1".into()),
                command: Some("ls -la".into()),
                aggregated_output: Some("file.txt\n".into()),
                exit_code: Some(0),
                status: Some("completed".into()),
            },
        };
        let wire = translate_codex_event(&event);
        match wire {
            Some(AgentEventWire::ToolResult { output, is_error }) => {
                assert_eq!(output, "file.txt\n");
                assert!(!is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn translate_codex_error() {
        let event = apiari_codex_sdk::Event::Error {
            message: Some("rate limited".into()),
        };
        let wire = translate_codex_event(&event);
        match wire {
            Some(AgentEventWire::Error { message }) => assert_eq!(message, "rate limited"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn translate_codex_turn_failed() {
        let event = apiari_codex_sdk::Event::TurnFailed {
            usage: None,
            error: Some(apiari_codex_sdk::types::ThreadError {
                message: Some("something broke".into()),
                code: None,
            }),
        };
        let wire = translate_codex_event(&event);
        match wire {
            Some(AgentEventWire::Error { message }) => assert_eq!(message, "something broke"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn translate_codex_unknown_event() {
        let event = apiari_codex_sdk::Event::Unknown;
        assert!(translate_codex_event(&event).is_none());
    }

    // ── Gemini event translation tests ──

    #[test]
    fn translate_gemini_agent_message() {
        let event = apiari_gemini_sdk::Event::ItemCompleted {
            item: apiari_gemini_sdk::Item::AgentMessage {
                id: Some("m1".into()),
                text: Some("hello world".into()),
            },
        };
        let wire = translate_gemini_event(&event);
        match wire {
            Some(AgentEventWire::TextDelta { text }) => assert_eq!(text, "hello world"),
            other => panic!("expected TextDelta, got {other:?}"),
        }
    }

    #[test]
    fn translate_gemini_command_started() {
        let event = apiari_gemini_sdk::Event::ItemStarted {
            item: apiari_gemini_sdk::Item::CommandExecution {
                id: Some("c1".into()),
                command: Some("ls -la".into()),
                aggregated_output: None,
                exit_code: None,
                status: None,
            },
        };
        let wire = translate_gemini_event(&event);
        match wire {
            Some(AgentEventWire::ToolUse { tool, input }) => {
                assert_eq!(tool, "Bash");
                assert_eq!(input, "ls -la");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn translate_gemini_command_completed() {
        let event = apiari_gemini_sdk::Event::ItemCompleted {
            item: apiari_gemini_sdk::Item::CommandExecution {
                id: Some("c1".into()),
                command: Some("ls -la".into()),
                aggregated_output: Some("file.txt\n".into()),
                exit_code: Some(0),
                status: Some("completed".into()),
            },
        };
        let wire = translate_gemini_event(&event);
        match wire {
            Some(AgentEventWire::ToolResult { output, is_error }) => {
                assert_eq!(output, "file.txt\n");
                assert!(!is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn translate_gemini_tool_call() {
        let event = apiari_gemini_sdk::Event::ItemCompleted {
            item: apiari_gemini_sdk::Item::ToolCall {
                id: Some("t1".into()),
                tool: Some("web_search".into()),
                status: None,
            },
        };
        let wire = translate_gemini_event(&event);
        match wire {
            Some(AgentEventWire::ToolUse { tool, input }) => {
                assert_eq!(tool, "web_search");
                assert!(input.is_empty());
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn translate_gemini_error() {
        let event = apiari_gemini_sdk::Event::Error {
            message: Some("rate limited".into()),
            status: None,
            fatal: None,
        };
        let wire = translate_gemini_event(&event);
        match wire {
            Some(AgentEventWire::Error { message }) => assert_eq!(message, "rate limited"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn translate_gemini_turn_failed() {
        let event = apiari_gemini_sdk::Event::TurnFailed {
            usage: None,
            error: Some(apiari_gemini_sdk::ThreadError {
                message: Some("something broke".into()),
                code: None,
            }),
        };
        let wire = translate_gemini_event(&event);
        match wire {
            Some(AgentEventWire::Error { message }) => assert_eq!(message, "something broke"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn translate_gemini_unknown_event() {
        let event = apiari_gemini_sdk::Event::Unknown;
        assert!(translate_gemini_event(&event).is_none());
    }
}
