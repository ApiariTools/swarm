use apiari_claude_sdk::types::ContentBlock;
pub use apiari_tui::conversation::ConversationEntry;
use chrono::Local;
use ratatui::layout::Rect;
use std::time::Instant;
use tokio::sync::mpsc;

/// Format a timestamp for display (e.g. "2:34 PM").
fn now_timestamp() -> String {
    Local::now().format("%-I:%M %p").to_string()
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
    /// Timestamp captured when streaming started (for the flushed entry).
    pub streaming_timestamp: String,
    /// Whether we're currently receiving streaming text.
    pub is_streaming: bool,
    /// Scroll offset (0 = bottom, follows output).
    pub scroll_offset: u32,
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
    /// Entry index of the currently focused ToolCall (for Tab/Enter navigation).
    pub focused_tool: Option<usize>,
    /// (start_line, line_count) per entry, built during render.
    pub entry_line_map: Vec<(u32, u32)>,
    /// Total logical line count from last render.
    pub total_rendered_lines: u32,
    /// Conversation area rect from last render (for mouse hit-testing).
    pub conversation_area: Rect,
    /// Whether the current turn included tool use (for status tracking).
    turn_had_tool_use: bool,
}

impl TuiApp {
    pub fn new(sdk_rx: mpsc::UnboundedReceiver<SdkEvent>) -> Self {
        Self {
            entries: Vec::new(),
            streaming_text: String::new(),
            streaming_timestamp: String::new(),
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
            focused_tool: None,
            entry_line_map: Vec::new(),
            total_rendered_lines: 0,
            conversation_area: Rect::default(),
            turn_had_tool_use: false,
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
                    self.streaming_timestamp = now_timestamp();
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
                            Some(ConversationEntry::AssistantText { text: existing, .. }) if existing == text
                        );
                        if !already_captured {
                            self.entries.push(ConversationEntry::AssistantText {
                                text: text.clone(),
                                timestamp: now_timestamp(),
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
                    self.turn_had_tool_use = true;
                    self.status = SessionStatus::ToolRunning;
                    self.entries.push(ConversationEntry::ToolCall {
                        tool: name.clone(),
                        input: input_str,
                        output: None,
                        is_error: false,
                        collapsed: true,
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
                if self.turn_had_tool_use {
                    // Tools were used — SDK will auto-continue with next turn.
                    self.status = SessionStatus::ToolRunning;
                }
                // No tool use: keep current status (Streaming/Thinking) until
                // Result or SessionWaiting arrives.
                self.turn_had_tool_use = false;
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
                    text: "Waiting for messages... (press i to type, or use `swarm send`)"
                        .to_string(),
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
            let timestamp = std::mem::take(&mut self.streaming_timestamp);
            self.entries
                .push(ConversationEntry::AssistantText { text, timestamp });
        }
        self.is_streaming = false;
    }

    /// Add a user message to the conversation.
    pub fn add_user_message(&mut self, text: String) {
        self.entries.push(ConversationEntry::User {
            text,
            timestamp: now_timestamp(),
        });
        self.status = SessionStatus::Streaming;
        self.turn_had_tool_use = false;
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

    // ── Tool collapse ──

    /// Toggle all tool blocks: if any are collapsed, expand all; otherwise collapse all.
    pub fn toggle_all_tools(&mut self) {
        let any_collapsed = self.entries.iter().any(|e| {
            matches!(
                e,
                ConversationEntry::ToolCall {
                    collapsed: true,
                    ..
                }
            )
        });
        let new_state = !any_collapsed;
        for entry in &mut self.entries {
            if let ConversationEntry::ToolCall { collapsed, .. } = entry {
                *collapsed = new_state;
            }
        }
    }

    // ── Individual tool focus ──

    /// Collect indices of ToolCall entries.
    fn tool_indices(&self) -> Vec<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                if matches!(e, ConversationEntry::ToolCall { .. }) {
                    Some(i)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Tab: focus the next ToolCall after current (wraps around).
    pub fn focus_next_tool(&mut self) {
        let tools = self.tool_indices();
        if tools.is_empty() {
            return;
        }
        self.focused_tool = Some(match self.focused_tool {
            None => tools[0],
            Some(cur) => {
                // Find first tool index strictly after `cur`
                match tools.iter().find(|&&i| i > cur) {
                    Some(&next) => next,
                    None => tools[0], // wrap
                }
            }
        });
    }

    /// Shift-Tab: focus the previous ToolCall (wraps around).
    pub fn focus_prev_tool(&mut self) {
        let tools = self.tool_indices();
        if tools.is_empty() {
            return;
        }
        self.focused_tool = Some(match self.focused_tool {
            None => *tools.last().unwrap(),
            Some(cur) => {
                // Find last tool index strictly before `cur`
                match tools.iter().rev().find(|&&i| i < cur) {
                    Some(&prev) => prev,
                    None => *tools.last().unwrap(), // wrap
                }
            }
        });
    }

    /// Enter: flip the collapsed state of the focused tool.
    pub fn toggle_focused_tool(&mut self) {
        if let Some(idx) = self.focused_tool
            && let Some(ConversationEntry::ToolCall { collapsed, .. }) = self.entries.get_mut(idx)
        {
            *collapsed = !*collapsed;
        }
    }

    /// Mouse click: flip tool at entry index and set focus.
    pub fn toggle_tool_at(&mut self, idx: usize) {
        if let Some(ConversationEntry::ToolCall { collapsed, .. }) = self.entries.get_mut(idx) {
            *collapsed = !*collapsed;
            self.focused_tool = Some(idx);
        }
    }

    /// Esc: clear the focused tool indicator.
    pub fn clear_focus(&mut self) {
        self.focused_tool = None;
    }

    /// After focus change, adjust scroll so the focused tool is visible.
    pub fn scroll_to_focused(&mut self) {
        let idx = match self.focused_tool {
            Some(i) => i,
            None => return,
        };
        if idx >= self.entry_line_map.len() {
            return;
        }
        let (start, count) = self.entry_line_map[idx];
        let visible = self.viewport_height as u32;
        if visible == 0 || self.total_rendered_lines <= visible {
            return;
        }
        let max_scroll = self.total_rendered_lines.saturating_sub(visible);
        // Current top line: when auto_scroll, top = max_scroll (bottom-pinned).
        // scroll_offset counts lines UP from the bottom, so top = max_scroll - scroll_offset.
        let top = if self.auto_scroll {
            max_scroll
        } else {
            max_scroll.saturating_sub(self.scroll_offset)
        };
        let bottom = top + visible;

        if start < top {
            // Need to scroll up to show the entry
            let new_top = start;
            self.scroll_offset = max_scroll.saturating_sub(new_top);
            self.auto_scroll = false;
        } else if start + count > bottom {
            // Need to scroll down to show the entry
            let new_top = (start + count).saturating_sub(visible);
            self.scroll_offset = max_scroll.saturating_sub(new_top);
            self.auto_scroll = self.scroll_offset == 0;
        }
    }

    /// Clear focus if index is out of bounds or no longer a ToolCall.
    pub fn validate_focus(&mut self) {
        if let Some(idx) = self.focused_tool {
            match self.entries.get(idx) {
                Some(ConversationEntry::ToolCall { .. }) => {} // valid
                _ => self.focused_tool = None,
            }
        }
    }

    /// Map a terminal row (from mouse click) to an entry index, if it's a ToolCall.
    pub fn entry_at_row(&self, row: u16) -> Option<usize> {
        // Convert terminal row to inner rect row
        let inner_top = self.conversation_area.y;
        let inner_height = self.conversation_area.height;
        if row < inner_top || row >= inner_top + inner_height {
            return None;
        }
        let visible_row = (row - inner_top) as u32;

        // Calculate the top logical line (same as in render)
        let visible = inner_height as u32;
        if self.total_rendered_lines == 0 {
            return None;
        }
        let max_scroll = self.total_rendered_lines.saturating_sub(visible);
        let top = if self.auto_scroll {
            max_scroll
        } else {
            max_scroll.saturating_sub(self.scroll_offset)
        };

        let logical_line = top + visible_row;

        // Binary search entry_line_map for the entry containing this logical line
        for (i, &(start, count)) in self.entry_line_map.iter().enumerate() {
            if logical_line >= start && logical_line < start + count {
                // Only return if it's a ToolCall
                if matches!(
                    self.entries.get(i),
                    Some(ConversationEntry::ToolCall { .. })
                ) {
                    return Some(i);
                }
                return None;
            }
        }
        None
    }

    // ── Scrolling ──

    pub fn scroll_up(&mut self, amount: u32) {
        self.scroll_offset = self.scroll_offset.saturating_add(amount);
        self.auto_scroll = false;
    }

    pub fn scroll_down(&mut self, amount: u32) {
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
        format!(
            "{}\n... ({} more lines)",
            kept.join("\n"),
            lines.len() - max_lines
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_app() -> (TuiApp, mpsc::UnboundedSender<SdkEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (TuiApp::new(rx), tx)
    }

    // ── truncate_output ──

    #[test]
    fn truncate_output_short() {
        assert_eq!(truncate_output("a\nb\nc", 5), "a\nb\nc");
    }

    #[test]
    fn truncate_output_exact_limit() {
        assert_eq!(truncate_output("a\nb\nc", 3), "a\nb\nc");
    }

    #[test]
    fn truncate_output_over_limit() {
        let result = truncate_output("1\n2\n3\n4\n5", 2);
        assert_eq!(result, "1\n2\n... (3 more lines)");
    }

    #[test]
    fn truncate_output_empty() {
        assert_eq!(truncate_output("", 5), "");
    }

    // ── Scrolling ──

    #[test]
    fn scroll_up_disables_auto_scroll() {
        let (mut app, _tx) = make_app();
        assert!(app.auto_scroll);
        app.scroll_up(5);
        assert_eq!(app.scroll_offset, 5);
        assert!(!app.auto_scroll);
    }

    #[test]
    fn scroll_down_to_zero_re_enables_auto_scroll() {
        let (mut app, _tx) = make_app();
        app.scroll_up(10);
        assert!(!app.auto_scroll);
        app.scroll_down(10);
        assert_eq!(app.scroll_offset, 0);
        assert!(app.auto_scroll);
    }

    #[test]
    fn scroll_down_partial_stays_manual() {
        let (mut app, _tx) = make_app();
        app.scroll_up(10);
        app.scroll_down(3);
        assert_eq!(app.scroll_offset, 7);
        assert!(!app.auto_scroll);
    }

    #[test]
    fn scroll_down_saturates_at_zero() {
        let (mut app, _tx) = make_app();
        app.scroll_up(2);
        app.scroll_down(100);
        assert_eq!(app.scroll_offset, 0);
        assert!(app.auto_scroll);
    }

    #[test]
    fn scroll_to_bottom_resets() {
        let (mut app, _tx) = make_app();
        app.scroll_up(50);
        app.scroll_to_bottom();
        assert_eq!(app.scroll_offset, 0);
        assert!(app.auto_scroll);
    }

    // ── Input handling ──

    #[test]
    fn input_char_ascii() {
        let (mut app, _tx) = make_app();
        app.input_char('h');
        app.input_char('i');
        assert_eq!(app.input_buffer, "hi");
        assert_eq!(app.input_cursor, 2);
    }

    #[test]
    fn input_char_multibyte_utf8() {
        let (mut app, _tx) = make_app();
        app.input_char('🐝');
        assert_eq!(app.input_buffer, "🐝");
        assert_eq!(app.input_cursor, 4); // 🐝 is 4 bytes
        app.input_char('!');
        assert_eq!(app.input_buffer, "🐝!");
        assert_eq!(app.input_cursor, 5);
    }

    #[test]
    fn input_backspace_ascii() {
        let (mut app, _tx) = make_app();
        app.input_char('a');
        app.input_char('b');
        app.input_backspace();
        assert_eq!(app.input_buffer, "a");
        assert_eq!(app.input_cursor, 1);
    }

    #[test]
    fn input_backspace_multibyte() {
        let (mut app, _tx) = make_app();
        app.input_char('a');
        app.input_char('é'); // 2-byte UTF-8
        app.input_backspace();
        assert_eq!(app.input_buffer, "a");
        assert_eq!(app.input_cursor, 1);
    }

    #[test]
    fn input_backspace_at_start_is_noop() {
        let (mut app, _tx) = make_app();
        app.input_backspace(); // should not panic
        assert_eq!(app.input_buffer, "");
        assert_eq!(app.input_cursor, 0);
    }

    #[test]
    fn input_cursor_movement() {
        let (mut app, _tx) = make_app();
        app.input_char('a');
        app.input_char('b');
        app.input_char('c');
        // cursor at end (3)
        app.input_cursor_left();
        assert_eq!(app.input_cursor, 2);
        app.input_cursor_left();
        assert_eq!(app.input_cursor, 1);
        app.input_cursor_right();
        assert_eq!(app.input_cursor, 2);
    }

    #[test]
    fn input_cursor_left_at_start_is_noop() {
        let (mut app, _tx) = make_app();
        app.input_char('x');
        app.input_cursor_left();
        app.input_cursor_left(); // already at 0
        assert_eq!(app.input_cursor, 0);
    }

    #[test]
    fn input_cursor_right_at_end_is_noop() {
        let (mut app, _tx) = make_app();
        app.input_char('x');
        app.input_cursor_right(); // already at end
        assert_eq!(app.input_cursor, 1);
    }

    #[test]
    fn input_cursor_movement_multibyte() {
        let (mut app, _tx) = make_app();
        app.input_char('a');
        app.input_char('🐝'); // 4 bytes
        app.input_char('b');
        // buffer: "a🐝b", cursor at 6
        app.input_cursor_left(); // back over 'b' → 5
        assert_eq!(app.input_cursor, 5);
        app.input_cursor_left(); // back over 🐝 → 1
        assert_eq!(app.input_cursor, 1);
        app.input_cursor_right(); // forward over 🐝 → 5
        assert_eq!(app.input_cursor, 5);
    }

    #[test]
    fn input_insert_at_middle() {
        let (mut app, _tx) = make_app();
        app.input_char('a');
        app.input_char('c');
        app.input_cursor_left(); // cursor between 'a' and 'c'
        app.input_char('b');
        assert_eq!(app.input_buffer, "abc");
    }

    #[test]
    fn take_input_returns_and_clears() {
        let (mut app, _tx) = make_app();
        app.input_char('h');
        app.input_char('i');
        let text = app.take_input();
        assert_eq!(text, "hi");
        assert_eq!(app.input_buffer, "");
        assert_eq!(app.input_cursor, 0);
    }

    // ── SDK event handling ──

    #[test]
    fn thinking_delta_sets_thinking_status() {
        let (mut app, tx) = make_app();
        tx.send(SdkEvent::ThinkingDelta).unwrap();
        app.drain_sdk_events();
        assert_eq!(app.status, SessionStatus::Thinking);
    }

    #[test]
    fn system_event_sets_model() {
        let (mut app, tx) = make_app();
        tx.send(SdkEvent::System {
            model: Some("opus-4".into()),
        })
        .unwrap();
        app.drain_sdk_events();
        assert_eq!(app.model, Some("opus-4".into()));
        assert_eq!(app.status, SessionStatus::Streaming);
    }

    #[test]
    fn text_delta_accumulates_streaming() {
        let (mut app, tx) = make_app();
        tx.send(SdkEvent::TextDelta("hello ".into())).unwrap();
        tx.send(SdkEvent::TextDelta("world".into())).unwrap();
        app.drain_sdk_events();
        assert_eq!(app.streaming_text, "hello world");
        assert!(app.is_streaming);
        assert_eq!(app.status, SessionStatus::Streaming);
        // No entries yet — not flushed
        assert!(app.entries.is_empty());
    }

    #[test]
    fn turn_complete_flushes_streaming_text() {
        let (mut app, tx) = make_app();
        tx.send(SdkEvent::TextDelta("streamed text".into()))
            .unwrap();
        tx.send(SdkEvent::TurnComplete).unwrap();
        app.drain_sdk_events();
        assert!(!app.is_streaming);
        assert_eq!(app.streaming_text, "");
        assert_eq!(app.turn_count, 1);
        assert_eq!(app.status, SessionStatus::Streaming); // keeps previous status until Result
        assert!(matches!(
            &app.entries[0],
            ConversationEntry::AssistantText { text, .. } if text == "streamed text"
        ));
    }

    #[test]
    fn content_block_text_deduplicates_streamed() {
        let (mut app, tx) = make_app();
        // Simulate streaming then ContentBlock::Text with same content
        tx.send(SdkEvent::TextDelta("hello".into())).unwrap();
        tx.send(SdkEvent::ContentBlock(ContentBlock::Text {
            text: "hello".into(),
        }))
        .unwrap();
        app.drain_sdk_events();
        // Should only have one entry (the flushed streaming text)
        assert_eq!(app.entries.len(), 1);
    }

    #[test]
    fn content_block_text_adds_when_not_streamed() {
        let (mut app, tx) = make_app();
        tx.send(SdkEvent::ContentBlock(ContentBlock::Text {
            text: "direct text".into(),
        }))
        .unwrap();
        app.drain_sdk_events();
        assert_eq!(app.entries.len(), 1);
        assert!(matches!(
            &app.entries[0],
            ConversationEntry::AssistantText { text, .. } if text == "direct text"
        ));
    }

    #[test]
    fn tool_use_creates_tool_call_entry() {
        let (mut app, tx) = make_app();
        tx.send(SdkEvent::ContentBlock(ContentBlock::ToolUse {
            id: "t1".into(),
            name: "Bash".into(),
            input: json!({"command": "ls -la"}),
        }))
        .unwrap();
        app.drain_sdk_events();
        assert_eq!(app.tool_count, 1);
        assert_eq!(app.status, SessionStatus::ToolRunning);
        assert!(matches!(
            &app.entries[0],
            ConversationEntry::ToolCall { tool, input, output: None, collapsed: true, .. }
            if tool == "Bash" && input == "ls -la"
        ));
    }

    #[test]
    fn tool_use_extracts_file_path() {
        let (mut app, tx) = make_app();
        tx.send(SdkEvent::ContentBlock(ContentBlock::ToolUse {
            id: "t1".into(),
            name: "Read".into(),
            input: json!({"file_path": "/src/main.rs"}),
        }))
        .unwrap();
        app.drain_sdk_events();
        assert!(matches!(
            &app.entries[0],
            ConversationEntry::ToolCall { input, .. } if input == "/src/main.rs"
        ));
    }

    #[test]
    fn tool_use_extracts_pattern() {
        let (mut app, tx) = make_app();
        tx.send(SdkEvent::ContentBlock(ContentBlock::ToolUse {
            id: "t1".into(),
            name: "Grep".into(),
            input: json!({"pattern": "fn main"}),
        }))
        .unwrap();
        app.drain_sdk_events();
        assert!(matches!(
            &app.entries[0],
            ConversationEntry::ToolCall { input, .. } if input == "fn main"
        ));
    }

    #[test]
    fn tool_result_updates_last_tool_call() {
        let (mut app, tx) = make_app();
        tx.send(SdkEvent::ContentBlock(ContentBlock::ToolUse {
            id: "t1".into(),
            name: "Bash".into(),
            input: json!({"command": "echo hi"}),
        }))
        .unwrap();
        tx.send(SdkEvent::ContentBlock(ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: Some(json!("hi")),
            is_error: Some(false),
        }))
        .unwrap();
        app.drain_sdk_events();
        assert!(matches!(
            &app.entries[0],
            ConversationEntry::ToolCall { output: Some(out), is_error: false, .. }
            if out == "hi"
        ));
        assert_eq!(app.status, SessionStatus::Streaming);
    }

    #[test]
    fn tool_result_error_flag() {
        let (mut app, tx) = make_app();
        tx.send(SdkEvent::ContentBlock(ContentBlock::ToolUse {
            id: "t1".into(),
            name: "Bash".into(),
            input: json!({"command": "exit 1"}),
        }))
        .unwrap();
        tx.send(SdkEvent::ContentBlock(ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: Some(json!("failed")),
            is_error: Some(true),
        }))
        .unwrap();
        app.drain_sdk_events();
        assert!(matches!(
            &app.entries[0],
            ConversationEntry::ToolCall { is_error: true, .. }
        ));
    }

    #[test]
    fn result_event_sets_done_status() {
        let (mut app, tx) = make_app();
        tx.send(SdkEvent::Result {
            turns: 5,
            cost_usd: Some(0.12),
            session_id: "sess-1".into(),
            is_error: false,
        })
        .unwrap();
        app.drain_sdk_events();
        assert_eq!(app.status, SessionStatus::Done);
        assert_eq!(app.turn_count, 5);
        assert_eq!(app.cost_usd, Some(0.12));
        assert_eq!(app.session_id, Some("sess-1".into()));
    }

    #[test]
    fn result_event_error_sets_errored() {
        let (mut app, tx) = make_app();
        tx.send(SdkEvent::Result {
            turns: 1,
            cost_usd: None,
            session_id: "s".into(),
            is_error: true,
        })
        .unwrap();
        app.drain_sdk_events();
        assert_eq!(app.status, SessionStatus::Errored);
    }

    #[test]
    fn session_waiting_sets_waiting_status() {
        let (mut app, tx) = make_app();
        tx.send(SdkEvent::SessionWaiting {
            session_id: "sess-w".into(),
        })
        .unwrap();
        app.drain_sdk_events();
        assert_eq!(app.status, SessionStatus::Waiting);
        assert_eq!(app.session_id, Some("sess-w".into()));
        assert!(matches!(
            &app.entries[0],
            ConversationEntry::Status { text } if text.contains("Waiting")
        ));
    }

    #[test]
    fn error_event_sets_errored_and_adds_status() {
        let (mut app, tx) = make_app();
        tx.send(SdkEvent::Error("something broke".into())).unwrap();
        app.drain_sdk_events();
        assert_eq!(app.status, SessionStatus::Errored);
        assert!(matches!(
            &app.entries[0],
            ConversationEntry::Status { text } if text == "Error: something broke"
        ));
    }

    #[test]
    fn error_flushes_streaming_text() {
        let (mut app, tx) = make_app();
        tx.send(SdkEvent::TextDelta("partial ".into())).unwrap();
        tx.send(SdkEvent::Error("crash".into())).unwrap();
        app.drain_sdk_events();
        // Streaming text should be flushed before error
        assert_eq!(app.entries.len(), 2);
        assert!(matches!(
            &app.entries[0],
            ConversationEntry::AssistantText { text, .. } if text == "partial "
        ));
        assert!(matches!(
            &app.entries[1],
            ConversationEntry::Status { text } if text.contains("crash")
        ));
    }

    // ── add_user_message ──

    #[test]
    fn add_user_message_sets_streaming() {
        let (mut app, _tx) = make_app();
        app.add_user_message("hello".into());
        assert_eq!(app.status, SessionStatus::Streaming);
        assert!(matches!(
            &app.entries[0],
            ConversationEntry::User { text, .. } if text == "hello"
        ));
    }

    // ── Tool collapse ──

    #[test]
    fn toggle_tools_expands_all_when_any_collapsed() {
        let (mut app, _tx) = make_app();
        app.entries.push(ConversationEntry::ToolCall {
            tool: "A".into(),
            input: "".into(),
            output: None,
            is_error: false,
            collapsed: true,
        });
        app.entries.push(ConversationEntry::ToolCall {
            tool: "B".into(),
            input: "".into(),
            output: None,
            is_error: false,
            collapsed: false,
        });
        app.toggle_all_tools();
        // Any was collapsed → expand all (collapsed = false)
        for entry in &app.entries {
            if let ConversationEntry::ToolCall { collapsed, .. } = entry {
                assert!(!collapsed);
            }
        }
    }

    #[test]
    fn toggle_tools_collapses_all_when_none_collapsed() {
        let (mut app, _tx) = make_app();
        app.entries.push(ConversationEntry::ToolCall {
            tool: "A".into(),
            input: "".into(),
            output: None,
            is_error: false,
            collapsed: false,
        });
        app.entries.push(ConversationEntry::ToolCall {
            tool: "B".into(),
            input: "".into(),
            output: None,
            is_error: false,
            collapsed: false,
        });
        app.toggle_all_tools();
        for entry in &app.entries {
            if let ConversationEntry::ToolCall { collapsed, .. } = entry {
                assert!(collapsed);
            }
        }
    }

    #[test]
    fn toggle_tools_no_tools_is_noop() {
        let (mut app, _tx) = make_app();
        app.entries.push(ConversationEntry::AssistantText {
            text: "hi".into(),
            timestamp: String::new(),
        });
        app.toggle_all_tools(); // should not panic
        assert_eq!(app.entries.len(), 1);
    }

    // ── drain_sdk_events ──

    #[test]
    fn drain_returns_forwarded_events() {
        let (mut app, tx) = make_app();
        tx.send(SdkEvent::ThinkingDelta).unwrap();
        tx.send(SdkEvent::TextDelta("x".into())).unwrap();
        let events = app.drain_sdk_events();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn drain_empty_channel_returns_empty() {
        let (mut app, _tx) = make_app();
        let events = app.drain_sdk_events();
        assert!(events.is_empty());
    }

    // ── Full conversation flow ──

    #[test]
    fn full_conversation_flow() {
        let (mut app, tx) = make_app();

        // System
        tx.send(SdkEvent::System {
            model: Some("opus".into()),
        })
        .unwrap();

        // Streaming response
        tx.send(SdkEvent::TextDelta("I'll ".into())).unwrap();
        tx.send(SdkEvent::TextDelta("help.".into())).unwrap();

        // Tool use
        tx.send(SdkEvent::ContentBlock(ContentBlock::ToolUse {
            id: "t1".into(),
            name: "Read".into(),
            input: json!({"file_path": "main.rs"}),
        }))
        .unwrap();

        tx.send(SdkEvent::ContentBlock(ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: Some(json!("fn main() {}")),
            is_error: Some(false),
        }))
        .unwrap();

        // More text
        tx.send(SdkEvent::TextDelta("Done!".into())).unwrap();
        tx.send(SdkEvent::TurnComplete).unwrap();

        // Result
        tx.send(SdkEvent::Result {
            turns: 3,
            cost_usd: Some(0.05),
            session_id: "s1".into(),
            is_error: false,
        })
        .unwrap();

        // Waiting
        tx.send(SdkEvent::SessionWaiting {
            session_id: "s1".into(),
        })
        .unwrap();

        app.drain_sdk_events();

        assert_eq!(app.model, Some("opus".into()));
        assert_eq!(app.tool_count, 1);
        assert_eq!(app.turn_count, 3);
        assert_eq!(app.cost_usd, Some(0.05));
        assert_eq!(app.session_id, Some("s1".into()));
        assert_eq!(app.status, SessionStatus::Waiting);

        // Check entries: AssistantText("I'll help."), ToolCall, AssistantText("Done!"), Status(waiting)
        assert_eq!(app.entries.len(), 4);
        assert!(matches!(
            &app.entries[0],
            ConversationEntry::AssistantText { text, .. } if text == "I'll help."
        ));
        assert!(matches!(
            &app.entries[1],
            ConversationEntry::ToolCall { tool, .. } if tool == "Read"
        ));
        assert!(matches!(
            &app.entries[2],
            ConversationEntry::AssistantText { text, .. } if text == "Done!"
        ));
        assert!(matches!(&app.entries[3], ConversationEntry::Status { .. }));
    }

    // ── tick ──

    #[test]
    fn tick_increments() {
        let (mut app, _tx) = make_app();
        assert_eq!(app.tick_count, 0);
        app.tick();
        app.tick();
        assert_eq!(app.tick_count, 2);
    }

    #[test]
    fn tick_wraps() {
        let (mut app, _tx) = make_app();
        app.tick_count = u64::MAX;
        app.tick();
        assert_eq!(app.tick_count, 0);
    }

    // ── Tool use flushes streaming ──

    #[test]
    fn tool_use_flushes_streaming_text() {
        let (mut app, tx) = make_app();
        tx.send(SdkEvent::TextDelta("before tool".into())).unwrap();
        tx.send(SdkEvent::ContentBlock(ContentBlock::ToolUse {
            id: "t1".into(),
            name: "Bash".into(),
            input: json!({"command": "ls"}),
        }))
        .unwrap();
        app.drain_sdk_events();
        // Streaming text flushed as entry[0], tool call as entry[1]
        assert_eq!(app.entries.len(), 2);
        assert!(matches!(
            &app.entries[0],
            ConversationEntry::AssistantText { text, .. } if text == "before tool"
        ));
        assert!(matches!(
            &app.entries[1],
            ConversationEntry::ToolCall { tool, .. } if tool == "Bash"
        ));
    }

    // ── Individual tool focus ──

    fn make_tool(name: &str) -> ConversationEntry {
        ConversationEntry::ToolCall {
            tool: name.into(),
            input: "test".into(),
            output: None,
            is_error: false,
            collapsed: true,
        }
    }

    fn make_text(s: &str) -> ConversationEntry {
        ConversationEntry::AssistantText {
            text: s.into(),
            timestamp: String::new(),
        }
    }

    #[test]
    fn focus_next_no_tools_is_noop() {
        let (mut app, _tx) = make_app();
        app.entries.push(make_text("hello"));
        app.focus_next_tool();
        assert_eq!(app.focused_tool, None);
    }

    #[test]
    fn focus_next_first_tool() {
        let (mut app, _tx) = make_app();
        app.entries.push(make_text("hello"));
        app.entries.push(make_tool("Bash"));
        app.focus_next_tool();
        assert_eq!(app.focused_tool, Some(1));
    }

    #[test]
    fn focus_next_wraps() {
        let (mut app, _tx) = make_app();
        app.entries.push(make_tool("Bash"));
        app.entries.push(make_text("text"));
        app.entries.push(make_tool("Read"));
        // Focus last tool
        app.focused_tool = Some(2);
        app.focus_next_tool();
        // Should wrap to first tool
        assert_eq!(app.focused_tool, Some(0));
    }

    #[test]
    fn focus_prev_wraps() {
        let (mut app, _tx) = make_app();
        app.entries.push(make_tool("Bash"));
        app.entries.push(make_text("text"));
        app.entries.push(make_tool("Read"));
        // Focus first tool
        app.focused_tool = Some(0);
        app.focus_prev_tool();
        // Should wrap to last tool
        assert_eq!(app.focused_tool, Some(2));
    }

    #[test]
    fn focus_prev_no_focus() {
        let (mut app, _tx) = make_app();
        app.entries.push(make_tool("Bash"));
        app.entries.push(make_tool("Read"));
        // No current focus
        app.focus_prev_tool();
        // Should go to last tool
        assert_eq!(app.focused_tool, Some(1));
    }

    #[test]
    fn focus_skips_non_tool_entries() {
        let (mut app, _tx) = make_app();
        app.entries.push(make_text("text1"));
        app.entries.push(make_tool("Bash")); // index 1
        app.entries.push(make_text("text2"));
        app.entries.push(ConversationEntry::Status {
            text: "status".into(),
        });
        app.entries.push(make_tool("Read")); // index 4
        app.entries.push(make_text("text3"));

        // First Tab → index 1
        app.focus_next_tool();
        assert_eq!(app.focused_tool, Some(1));
        // Second Tab → index 4 (skips text and status)
        app.focus_next_tool();
        assert_eq!(app.focused_tool, Some(4));
        // Third Tab → wraps to index 1
        app.focus_next_tool();
        assert_eq!(app.focused_tool, Some(1));
    }

    #[test]
    fn toggle_focused_tool() {
        let (mut app, _tx) = make_app();
        app.entries.push(make_tool("Bash"));
        app.focused_tool = Some(0);
        // Initially collapsed
        assert!(matches!(
            &app.entries[0],
            ConversationEntry::ToolCall {
                collapsed: true,
                ..
            }
        ));
        app.toggle_focused_tool();
        assert!(matches!(
            &app.entries[0],
            ConversationEntry::ToolCall {
                collapsed: false,
                ..
            }
        ));
        app.toggle_focused_tool();
        assert!(matches!(
            &app.entries[0],
            ConversationEntry::ToolCall {
                collapsed: true,
                ..
            }
        ));
    }

    #[test]
    fn clear_focus_test() {
        let (mut app, _tx) = make_app();
        app.entries.push(make_tool("Bash"));
        app.focused_tool = Some(0);
        app.clear_focus();
        assert_eq!(app.focused_tool, None);
    }

    #[test]
    fn validate_focus_clears_invalid() {
        let (mut app, _tx) = make_app();
        app.entries.push(make_tool("Bash"));
        app.focused_tool = Some(5); // out of bounds
        app.validate_focus();
        assert_eq!(app.focused_tool, None);
    }

    #[test]
    fn validate_focus_clears_non_tool() {
        let (mut app, _tx) = make_app();
        app.entries.push(make_text("hello"));
        app.focused_tool = Some(0); // points to text, not tool
        app.validate_focus();
        assert_eq!(app.focused_tool, None);
    }

    #[test]
    fn validate_focus_keeps_valid() {
        let (mut app, _tx) = make_app();
        app.entries.push(make_tool("Bash"));
        app.focused_tool = Some(0);
        app.validate_focus();
        assert_eq!(app.focused_tool, Some(0));
    }
}
