use apiari_claude_sdk::types::ContentBlock;
use std::time::Instant;
use tokio::sync::mpsc;

/// A rendered conversation entry in the TUI.
#[derive(Debug, Clone)]
pub enum ConversationEntry {
    /// User message.
    User { text: String },
    /// Assistant text block (may be streamed incrementally).
    AssistantText { text: String },
    /// A tool call with its result.
    ToolCall {
        tool: String,
        input: String,
        output: Option<String>,
        is_error: bool,
    },
    /// Status message (e.g. "Session started", "Rate limited").
    Status { text: String },
}

/// An SDK event forwarded from the background session task.
#[derive(Debug)]
pub enum SdkEvent {
    /// Streaming text delta.
    TextDelta(String),
    /// A thinking delta (model is reasoning).
    ThinkingDelta,
    /// A complete content block.
    ContentBlock(ContentBlock),
    /// Message assembly complete (one assistant turn done).
    TurnComplete,
    /// Session result (final).
    Result {
        turns: u64,
        cost_usd: Option<f64>,
        session_id: String,
        is_error: bool,
    },
    /// Error from the SDK.
    Error(String),
    /// System message (session metadata).
    System { model: Option<String> },
    /// Session complete, now waiting for follow-up messages.
    SessionWaiting { session_id: String },
}

/// Current status of the agent session.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionStatus {
    /// Waiting for session to start.
    Starting,
    /// Model is thinking (extended thinking).
    Thinking,
    /// Streaming response from model.
    Streaming,
    /// Waiting for tool execution.
    ToolRunning,
    /// Model turn complete, waiting for user or next turn.
    Idle,
    /// Session finished, waiting for follow-up messages.
    Waiting,
    /// Session finished.
    Done,
    /// Session errored.
    Errored,
}

/// Input mode for the TUI.
#[derive(Debug, Clone, PartialEq)]
pub enum InputMode {
    /// Normal — scroll, quit.
    Normal,
    /// Typing a follow-up message.
    Input,
}

/// The application state for the agent TUI.
pub struct TuiApp {
    /// Conversation history.
    pub entries: Vec<ConversationEntry>,
    /// Current streaming text buffer (appended to on TextDelta).
    pub streaming_text: String,
    /// Whether we're currently receiving streaming text.
    pub is_streaming: bool,
    /// Scroll offset (0 = bottom, follows output).
    pub scroll_offset: u16,
    /// Whether auto-scroll is active (disabled when user scrolls up).
    pub auto_scroll: bool,
    /// Session status.
    pub status: SessionStatus,
    /// Input mode.
    pub input_mode: InputMode,
    /// Input buffer for follow-up messages.
    pub input_buffer: String,
    /// Cursor position in input buffer.
    pub input_cursor: usize,
    /// Tool call count.
    pub tool_count: u32,
    /// Turn count.
    pub turn_count: u32,
    /// Model name (from system message).
    pub model: Option<String>,
    /// Session ID (from result).
    pub session_id: Option<String>,
    /// Total cost.
    pub cost_usd: Option<f64>,
    /// Receiver for SDK events.
    pub sdk_rx: mpsc::UnboundedReceiver<SdkEvent>,
    /// Total height available for rendering (updated each frame).
    pub viewport_height: u16,
    /// Timestamp of last SDK event (for stalled detection).
    pub last_event_at: Instant,
    /// Tick counter for animations.
    pub tick_count: u64,
}

impl TuiApp {
    pub fn new(sdk_rx: mpsc::UnboundedReceiver<SdkEvent>) -> Self {
        Self {
            entries: Vec::new(),
            streaming_text: String::new(),
            is_streaming: false,
            scroll_offset: 0,
            auto_scroll: true,
            status: SessionStatus::Starting,
            input_mode: InputMode::Normal,
            input_buffer: String::new(),
            input_cursor: 0,
            tool_count: 0,
            turn_count: 0,
            model: None,
            session_id: None,
            cost_usd: None,
            sdk_rx,
            viewport_height: 0,
            last_event_at: Instant::now(),
            tick_count: 0,
        }
    }

    /// Process pending SDK events from the channel.
    pub fn drain_sdk_events(&mut self) -> Vec<SdkEvent> {
        let mut forwarded = Vec::new();
        while let Ok(event) = self.sdk_rx.try_recv() {
            self.handle_sdk_event(&event);
            forwarded.push(event);
        }
        forwarded
    }

    fn handle_sdk_event(&mut self, event: &SdkEvent) {
        self.last_event_at = Instant::now();
        match event {
            SdkEvent::ThinkingDelta => {
                self.status = SessionStatus::Thinking;
            }
            SdkEvent::System { model } => {
                self.model = model.clone();
                self.status = SessionStatus::Streaming;
            }
            SdkEvent::TextDelta(text) => {
                if !self.is_streaming {
                    // Flush any previous streaming text
                    self.flush_streaming_text();
                    self.is_streaming = true;
                    self.status = SessionStatus::Streaming;
                }
                self.streaming_text.push_str(text);
            }
            SdkEvent::ContentBlock(block) => match block {
                ContentBlock::Text { text } => {
                    if self.is_streaming {
                        self.flush_streaming_text();
                    } else {
                        // Avoid duplicating text already captured via streaming
                        let already_captured = matches!(
                            self.entries.last(),
                            Some(ConversationEntry::AssistantText { text: existing }) if existing == text
                        );
                        if !already_captured {
                            self.entries.push(ConversationEntry::AssistantText {
                                text: text.clone(),
                            });
                        }
                    }
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    self.flush_streaming_text();
                    let input_str = if let Some(obj) = input.as_object() {
                        // Try to show a compact version of the input
                        if let Some(cmd) = obj.get("command").and_then(|v| v.as_str()) {
                            cmd.to_string()
                        } else if let Some(path) = obj.get("file_path").and_then(|v| v.as_str()) {
                            path.to_string()
                        } else if let Some(pattern) = obj.get("pattern").and_then(|v| v.as_str()) {
                            pattern.to_string()
                        } else {
                            serde_json::to_string_pretty(input)
                                .unwrap_or_else(|_| input.to_string())
                        }
                    } else {
                        input.to_string()
                    };
                    self.tool_count += 1;
                    self.status = SessionStatus::ToolRunning;
                    self.entries.push(ConversationEntry::ToolCall {
                        tool: name.clone(),
                        input: input_str,
                        output: None,
                        is_error: false,
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
                    let is_err = is_error.unwrap_or(false);
                    // Update the last ToolCall entry with the result
                    if let Some(ConversationEntry::ToolCall {
                        output: o,
                        is_error: e,
                        ..
                    }) = self.entries.last_mut()
                    {
                        *o = Some(truncate_output(&output, 20));
                        *e = is_err;
                    }
                    self.status = SessionStatus::Streaming;
                }
                ContentBlock::Thinking { .. } => {
                    // We don't display thinking blocks in the TUI
                }
            },
            SdkEvent::TurnComplete => {
                self.flush_streaming_text();
                self.turn_count += 1;
                self.status = SessionStatus::Idle;
            }
            SdkEvent::Result {
                turns,
                cost_usd,
                session_id,
                is_error,
            } => {
                self.flush_streaming_text();
                self.turn_count = *turns as u32;
                self.cost_usd = *cost_usd;
                self.session_id = Some(session_id.clone());
                self.status = if *is_error {
                    SessionStatus::Errored
                } else {
                    SessionStatus::Done
                };
            }
            SdkEvent::SessionWaiting { session_id } => {
                self.session_id = Some(session_id.clone());
                self.status = SessionStatus::Waiting;
                self.entries.push(ConversationEntry::Status {
                    text: "Waiting for messages... (press i to type, or use `swarm send`)".to_string(),
                });
            }
            SdkEvent::Error(msg) => {
                self.flush_streaming_text();
                self.entries.push(ConversationEntry::Status {
                    text: format!("Error: {}", msg),
                });
                self.status = SessionStatus::Errored;
            }
        }
    }

    /// Flush accumulated streaming text into a conversation entry.
    fn flush_streaming_text(&mut self) {
        if !self.streaming_text.is_empty() {
            let text = std::mem::take(&mut self.streaming_text);
            self.entries.push(ConversationEntry::AssistantText { text });
        }
        self.is_streaming = false;
    }

    /// Add a user message to the conversation.
    pub fn add_user_message(&mut self, text: String) {
        self.entries.push(ConversationEntry::User { text });
        self.status = SessionStatus::Streaming;
    }

    // ── Input handling ──

    pub fn input_char(&mut self, c: char) {
        self.input_buffer.insert(self.input_cursor, c);
        self.input_cursor += c.len_utf8();
    }

    pub fn input_backspace(&mut self) {
        if self.input_cursor > 0 {
            let prev = self.input_buffer[..self.input_cursor]
                .chars()
                .last()
                .map(|c| c.len_utf8())
                .unwrap_or(0);
            self.input_cursor -= prev;
            self.input_buffer.remove(self.input_cursor);
        }
    }

    pub fn input_cursor_left(&mut self) {
        if self.input_cursor > 0 {
            let prev = self.input_buffer[..self.input_cursor]
                .chars()
                .last()
                .map(|c| c.len_utf8())
                .unwrap_or(0);
            self.input_cursor -= prev;
        }
    }

    pub fn input_cursor_right(&mut self) {
        if self.input_cursor < self.input_buffer.len() {
            let next = self.input_buffer[self.input_cursor..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(0);
            self.input_cursor += next;
        }
    }

    /// Take the input buffer contents and reset.
    pub fn take_input(&mut self) -> String {
        self.input_cursor = 0;
        std::mem::take(&mut self.input_buffer)
    }

    // ── Scrolling ──

    pub fn scroll_up(&mut self, amount: u16) {
        self.scroll_offset = self.scroll_offset.saturating_add(amount);
        self.auto_scroll = false;
    }

    pub fn scroll_down(&mut self, amount: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
        if self.scroll_offset == 0 {
            self.auto_scroll = true;
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
        self.auto_scroll = true;
    }

    /// Advance the animation tick counter.
    pub fn tick(&mut self) {
        self.tick_count = self.tick_count.wrapping_add(1);
    }
}

/// Truncate tool output to a maximum number of lines.
fn truncate_output(output: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = output.lines().collect();
    if lines.len() <= max_lines {
        output.to_string()
    } else {
        let kept: Vec<&str> = lines[..max_lines].to_vec();
        format!("{}\n... ({} more lines)", kept.join("\n"), lines.len() - max_lines)
    }
}
