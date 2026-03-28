use crate::agent_tui::app::ConversationEntry;
use crate::core::agent::AgentKind;
use crate::core::modifier::ModifierPrompt;
use crate::core::state::WorkerPhase;
use crate::daemon::protocol::{AgentEventWire, WorkerInfo};
use ratatui::prelude::Rect;
use std::cell::Cell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

/// Agents available in the daemon TUI.
pub fn daemon_agents() -> Vec<AgentKind> {
    vec![AgentKind::Claude, AgentKind::Codex, AgentKind::Gemini]
}

/// Which panel has focus.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Panel {
    Sidebar,
    Conversation,
}

/// TUI interaction mode.
#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Normal,
    Input,
    CreatePrompt,
    RepoSelect,
    AgentSelect,
    ModifierSelect,
    Confirm,
    Help,
    PrDetail,
}

/// PR detail info for the overlay.
#[derive(Debug, Clone)]
pub struct PrDetailInfo {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub url: String,
    #[allow(dead_code)] // used in tests; kept for future use
    pub worker_id: String,
}

impl PrDetailInfo {
    /// Construct from a WorkerInfo, returns None if no PR.
    pub fn from_worker(w: &WorkerInfo) -> Option<Self> {
        let url = w.pr_url.as_ref()?;
        let number = w
            .pr_number
            .or_else(|| url.rsplit('/').next().and_then(|s| s.parse().ok()))
            .unwrap_or(0);
        Some(Self {
            number,
            title: w
                .pr_title
                .clone()
                .unwrap_or_else(|| format!("PR #{}", number)),
            state: w.pr_state.clone().unwrap_or_else(|| "OPEN".to_string()),
            url: url.clone(),
            worker_id: w.id.clone(),
        })
    }
}

/// A pending action awaiting confirmation.
#[derive(Debug, Clone)]
pub enum PendingAction {
    Close(String),
    Merge(String),
}

/// Per-worker conversation state (mirrors patterns from agent_tui::app).
pub struct WorkerConversation {
    pub entries: Vec<ConversationEntry>,
    pub streaming_text: String,
    pub is_streaming: bool,
    pub scroll_offset: u32,
    pub auto_scroll: bool,
    pub filter_noise: bool,
    pub focused_tool: Option<usize>,
    pub tool_count: u32,
    pub turn_count: u32,
    pub cost_usd: Option<f64>,
    /// (start_line, line_count) per entry, built during render (raw lines).
    pub entry_line_map: Vec<(u32, u32)>,
    pub total_rendered_lines: u32,
    /// (start_visual_line, visual_line_count) per entry, accounting for wrap.
    pub entry_visual_line_map: Vec<(u32, u32)>,
    pub total_visual_lines: u32,
    /// Inner conversation area rect, set during render for hit-testing.
    pub conversation_area: Rect,
}

impl WorkerConversation {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            streaming_text: String::new(),
            is_streaming: false,
            scroll_offset: 0,
            auto_scroll: true,
            filter_noise: false,
            focused_tool: None,
            tool_count: 0,
            turn_count: 0,
            cost_usd: None,
            entry_line_map: Vec::new(),
            total_rendered_lines: 0,
            entry_visual_line_map: Vec::new(),
            total_visual_lines: 0,
            conversation_area: Rect::default(),
        }
    }

    /// Process an AgentEventWire into conversation entries.
    pub fn handle_event(&mut self, event: &AgentEventWire) {
        match event {
            AgentEventWire::TextDelta { text } => {
                if !self.is_streaming {
                    self.flush_streaming_text();
                    self.is_streaming = true;
                }
                self.streaming_text.push_str(text);
            }
            AgentEventWire::ThinkingDelta { .. } => {
                // We don't display thinking text in the TUI
            }
            AgentEventWire::ToolUse { tool, input } => {
                self.flush_streaming_text();
                self.tool_count += 1;
                let extracted = extract_tool_input(input);
                self.entries.push(ConversationEntry::ToolCall {
                    tool: tool.clone(),
                    input: extracted,
                    output: None,
                    is_error: false,
                    collapsed: true,
                });
            }
            AgentEventWire::ToolResult { output, is_error } => {
                if let Some(ConversationEntry::ToolCall {
                    output: o,
                    is_error: e,
                    ..
                }) = self.entries.last_mut()
                {
                    *o = Some(output.clone());
                    *e = *is_error;
                }
            }
            AgentEventWire::TurnComplete => {
                self.flush_streaming_text();
                self.turn_count += 1;
            }
            AgentEventWire::SessionResult {
                turns,
                cost_usd,
                session_id: _,
            } => {
                self.flush_streaming_text();
                self.turn_count = *turns as u32;
                self.cost_usd = *cost_usd;
            }
            AgentEventWire::SessionWaiting { .. } => {
                self.flush_streaming_text();
                self.entries.push(ConversationEntry::Status {
                    text: "Waiting for input...".to_string(),
                });
            }
            AgentEventWire::Error { message } => {
                self.flush_streaming_text();
                self.entries.push(ConversationEntry::Status {
                    text: format!("Error: {}", message),
                });
            }
        }
    }

    pub fn flush_streaming_text(&mut self) {
        if !self.streaming_text.is_empty() {
            let text = std::mem::take(&mut self.streaming_text);
            let timestamp = chrono::Local::now().format("%-I:%M %p").to_string();
            self.entries
                .push(ConversationEntry::AssistantText { text, timestamp });
        }
        self.is_streaming = false;
    }

    // ── Scrolling ──

    pub fn scroll_up(&mut self, amount: u32) {
        // viewport_height is set during render; use conversation_area as fallback
        let vh = if self.conversation_area.height > 0 {
            self.conversation_area.height as u32
        } else {
            1
        };
        let max = self.total_visual_lines.saturating_sub(vh);
        self.scroll_offset = self.scroll_offset.saturating_add(amount).min(max);
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

    // ── Tool collapse ──

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

    // ── Tool focus navigation ──

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

    /// Tab: focus the next ToolCall (wraps around).
    pub fn focus_next_tool(&mut self) {
        let tools = self.tool_indices();
        if tools.is_empty() {
            return;
        }
        self.focused_tool = Some(match self.focused_tool {
            None => tools[0],
            Some(cur) => match tools.iter().find(|&&i| i > cur) {
                Some(&next) => next,
                None => tools[0],
            },
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
            Some(cur) => match tools.iter().rev().find(|&&i| i < cur) {
                Some(&prev) => prev,
                None => *tools.last().unwrap(),
            },
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
    pub fn scroll_to_focused(&mut self, viewport_height: u16) {
        let idx = match self.focused_tool {
            Some(i) => i,
            None => return,
        };
        // Use visual line map if available, fall back to raw
        let (map, total) = if !self.entry_visual_line_map.is_empty() {
            (&self.entry_visual_line_map, self.total_visual_lines)
        } else {
            (&self.entry_line_map, self.total_rendered_lines)
        };
        if idx >= map.len() {
            return;
        }
        let (start, count) = map[idx];
        let visible = viewport_height as u32;
        if visible == 0 || total <= visible {
            return;
        }
        let max_scroll = total.saturating_sub(visible);
        let top = if self.auto_scroll {
            max_scroll
        } else {
            max_scroll.saturating_sub(self.scroll_offset)
        };
        let bottom = top + visible;

        if start < top {
            let new_top = start;
            self.scroll_offset = max_scroll.saturating_sub(new_top);
            self.auto_scroll = false;
        } else if start + count > bottom {
            let new_top = (start + count).saturating_sub(visible);
            self.scroll_offset = max_scroll.saturating_sub(new_top);
            self.auto_scroll = false;
        }
    }

    /// Clear focused_tool if index is out of bounds or no longer a ToolCall.
    pub fn validate_focus(&mut self) {
        if let Some(idx) = self.focused_tool {
            match self.entries.get(idx) {
                Some(ConversationEntry::ToolCall { .. }) => {}
                _ => self.focused_tool = None,
            }
        }
    }

    /// Map a terminal row (from mouse click) to an entry index, if it's a ToolCall.
    pub fn entry_at_row(&self, row: u16) -> Option<usize> {
        let inner_top = self.conversation_area.y;
        let inner_height = self.conversation_area.height;
        if row < inner_top || row >= inner_top + inner_height {
            return None;
        }
        let visible_row = (row - inner_top) as u32;

        // Use visual line map if available, fall back to raw
        let (map, total) = if !self.entry_visual_line_map.is_empty() {
            (&self.entry_visual_line_map, self.total_visual_lines)
        } else {
            (&self.entry_line_map, self.total_rendered_lines)
        };

        let visible = inner_height as u32;
        if total == 0 {
            return None;
        }
        let max_scroll = total.saturating_sub(visible);
        let top = if self.auto_scroll {
            max_scroll
        } else {
            max_scroll.saturating_sub(self.scroll_offset)
        };

        let logical_line = top + visible_row;

        for (i, &(start, count)) in map.iter().enumerate() {
            if logical_line >= start && logical_line < start + count {
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
}

/// The main application state for the daemon TUI.
pub struct DaemonTuiApp {
    pub workers: Vec<WorkerInfo>,
    pub selected: usize,
    pub list_scroll: Cell<usize>,

    // Per-worker conversation state
    pub conversations: HashMap<String, WorkerConversation>,

    // Panel focus & mode
    pub focus: Panel,
    pub mode: Mode,

    // Input
    pub input_buffer: String,
    pub input_cursor: usize,

    // Create-worker flow
    pub repos: Vec<PathBuf>,
    pub repo_select_index: usize,
    pub agent_select_index: usize,
    pub pending_prompt: String,
    pub pending_repo: Option<String>,
    pub modifier_prompts: Vec<ModifierPrompt>,
    pub modifier_selected: Vec<bool>,
    pub modifier_cursor: usize,

    // Confirm flow
    pub confirm_message: String,
    pub pending_action: Option<PendingAction>,

    // PR detail overlay
    pub pr_detail: Option<PrDetailInfo>,

    // Status
    pub status_message: Option<(String, Instant)>,
    pub connected: bool,
    pub work_dir: PathBuf,

    // Reconnection
    pub reconnect_at: Option<Instant>,
    pub is_remote: bool,

    // Animation
    pub tick_count: u64,

    // Viewport
    pub viewport_height: u16,

    // Dirty-flag rendering: only redraw when state changes
    pub needs_redraw: bool,

    // Lazy history loading: track which workers have had history fetched
    pub history_loaded: std::collections::HashSet<String>,

    // Queue of worktree IDs awaiting GetHistory responses (FIFO)
    pub pending_history: std::collections::VecDeque<String>,

    // Lazy-loaded prompts: only loaded when user enters create flow
    pub prompts_loaded: bool,

    // Zoom: hide sidebar and show conversation full-width
    pub zoomed: bool,
}

impl DaemonTuiApp {
    pub fn new(work_dir: PathBuf) -> Self {
        Self {
            workers: Vec::new(),
            selected: 0,
            list_scroll: Cell::new(0),
            conversations: HashMap::new(),
            focus: Panel::Sidebar,
            mode: Mode::Normal,
            input_buffer: String::new(),
            input_cursor: 0,
            repos: Vec::new(),
            repo_select_index: 0,
            agent_select_index: 0,
            pending_prompt: String::new(),
            pending_repo: None,
            modifier_prompts: Vec::new(),
            modifier_selected: Vec::new(),
            modifier_cursor: 0,
            confirm_message: String::new(),
            pending_action: None,
            pr_detail: None,
            status_message: None,
            connected: false,
            work_dir,
            reconnect_at: None,
            is_remote: false,
            tick_count: 0,
            viewport_height: 0,
            needs_redraw: true,
            history_loaded: std::collections::HashSet::new(),
            pending_history: std::collections::VecDeque::new(),
            prompts_loaded: false,
            zoomed: false,
        }
    }

    /// Update the worker list from daemon ListWorkers response.
    pub fn update_worker_list(&mut self, new_workers: Vec<WorkerInfo>) {
        self.workers = new_workers;

        // Clamp selection
        if !self.workers.is_empty() && self.selected >= self.workers.len() {
            self.selected = self.workers.len() - 1;
        }
        // Ensure conversation state exists for each worker, injecting initial prompt
        for w in &self.workers {
            self.conversations.entry(w.id.clone()).or_insert_with(|| {
                let mut conv = WorkerConversation::new();
                if !w.prompt.is_empty() {
                    conv.entries.push(ConversationEntry::User {
                        text: w.prompt.clone(),
                        timestamp: chrono::Local::now().format("%-I:%M %p").to_string(),
                    });
                }
                conv
            });
        }
        self.needs_redraw = true;
    }

    /// Handle an agent event from the daemon subscription.
    pub fn handle_agent_event(&mut self, worktree_id: &str, event: &AgentEventWire) {
        let conv = self
            .conversations
            .entry(worktree_id.to_string())
            .or_insert_with(WorkerConversation::new);
        conv.handle_event(event);
        self.needs_redraw = true;
    }

    /// Handle a phase change notification.
    pub fn handle_phase_change(&mut self, worktree_id: &str, phase: &WorkerPhase) {
        if let Some(w) = self.workers.iter_mut().find(|w| w.id == worktree_id) {
            w.phase = phase.clone();
            self.needs_redraw = true;
        }
    }

    /// Get the currently selected worker.
    pub fn selected_worker(&self) -> Option<&WorkerInfo> {
        self.workers.get(self.selected)
    }

    /// Alias for selected_worker (kept for call sites that distinguish parent context).
    pub fn selected_parent(&self) -> Option<&WorkerInfo> {
        self.workers.get(self.selected)
    }

    /// Get the conversation for the focused worker.
    pub fn selected_conversation(&self) -> Option<&WorkerConversation> {
        self.selected_worker()
            .and_then(|w| self.conversations.get(&w.id))
    }

    /// Get a mutable conversation for the focused worker.
    pub fn selected_conversation_mut(&mut self) -> Option<&mut WorkerConversation> {
        let id = self.selected_worker()?.id.clone();
        self.conversations.get_mut(&id)
    }

    /// Get the current status message (if not expired).
    pub fn current_status(&self) -> Option<&str> {
        self.status_message.as_ref().and_then(|(msg, at)| {
            if at.elapsed() < std::time::Duration::from_secs(5) {
                Some(msg.as_str())
            } else {
                None
            }
        })
    }

    /// Set a temporary status message.
    pub fn set_status(&mut self, msg: String) {
        self.status_message = Some((msg, Instant::now()));
        self.needs_redraw = true;
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

    pub fn take_input(&mut self) -> String {
        self.input_cursor = 0;
        std::mem::take(&mut self.input_buffer)
    }

    // ── Worker selection ──

    pub fn select_next(&mut self) {
        if !self.workers.is_empty() && self.selected + 1 < self.workers.len() {
            self.selected += 1;
            self.needs_redraw = true;
        }
    }

    pub fn select_prev(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            self.needs_redraw = true;
        }
    }

    pub fn tick(&mut self) {
        self.tick_count = self.tick_count.wrapping_add(1);
        self.needs_redraw = true;
    }

    /// Load modifier prompts on first use (avoids startup I/O).
    pub fn ensure_prompts_loaded(&mut self) {
        if !self.prompts_loaded {
            self.modifier_prompts = ModifierPrompt::available(&self.work_dir);
            self.modifier_selected = vec![false; self.modifier_prompts.len()];
            self.prompts_loaded = true;
            tui_log!(
                &self.work_dir,
                "prompts loaded: {} modifiers",
                self.modifier_prompts.len()
            );
        }
    }
}

/// Returns true for read-only / low-signal tools that can be filtered out.
pub fn is_noise_tool(tool: &str) -> bool {
    matches!(
        tool,
        "Read" | "Glob" | "Grep" | "WebFetch" | "WebSearch" | "TodoRead" | "TodoWrite" | "Explore"
    )
}

/// Extract a human-readable summary from raw JSON tool input.
///
/// Mirrors the agent_tui logic: tries `command`, `file_path`, `pattern`,
/// `old_string`, `query`, `prompt` keys in order, then falls back to
/// pretty-printed JSON.
pub fn extract_tool_input(raw_json: &str) -> String {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(raw_json) else {
        return raw_json.to_string();
    };
    if let Some(obj) = val.as_object() {
        for key in &[
            "command",
            "file_path",
            "pattern",
            "old_string",
            "query",
            "prompt",
        ] {
            if let Some(s) = obj.get(*key).and_then(|v| v.as_str()) {
                return s.to_string();
            }
        }
        serde_json::to_string_pretty(&val).unwrap_or_else(|_| raw_json.to_string())
    } else {
        raw_json.to_string()
    }
}

/// A history entry: either a wire event or a user message (which needs
/// different handling since user messages aren't part of the streaming protocol).
pub enum HistoryEntry {
    Event(AgentEventWire),
    UserMessage(String),
}

/// Parse events.jsonl content into displayable history entries.
///
/// The file stores `AgentEvent` (with timestamps), but the TUI conversation
/// works with `AgentEventWire` (streaming protocol). Convert on the fly.
pub fn parse_history_events(content: &str) -> Vec<HistoryEntry> {
    use crate::agent_tui::events::AgentEvent;

    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let event: AgentEvent = serde_json::from_str(line).ok()?;
            match event {
                AgentEvent::AssistantText { text, .. } => {
                    Some(HistoryEntry::Event(AgentEventWire::TextDelta { text }))
                }
                AgentEvent::ToolUse { tool, input, .. } => {
                    Some(HistoryEntry::Event(AgentEventWire::ToolUse { tool, input }))
                }
                AgentEvent::ToolResult {
                    output, is_error, ..
                } => Some(HistoryEntry::Event(AgentEventWire::ToolResult {
                    output,
                    is_error,
                })),
                AgentEvent::SessionResult {
                    turns,
                    cost_usd,
                    session_id,
                    ..
                } => Some(HistoryEntry::Event(AgentEventWire::SessionResult {
                    turns,
                    cost_usd,
                    session_id,
                })),
                AgentEvent::Error { message, .. } => {
                    Some(HistoryEntry::Event(AgentEventWire::Error { message }))
                }
                AgentEvent::UserMessage { text, .. } => Some(HistoryEntry::UserMessage(text)),
                AgentEvent::Start { .. } => None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_handles_text_delta() {
        let mut conv = WorkerConversation::new();
        conv.handle_event(&AgentEventWire::TextDelta {
            text: "hello ".into(),
        });
        conv.handle_event(&AgentEventWire::TextDelta {
            text: "world".into(),
        });
        assert_eq!(conv.streaming_text, "hello world");
        assert!(conv.is_streaming);
    }

    #[test]
    fn conversation_flushes_on_turn_complete() {
        let mut conv = WorkerConversation::new();
        conv.handle_event(&AgentEventWire::TextDelta {
            text: "done".into(),
        });
        conv.handle_event(&AgentEventWire::TurnComplete);
        assert_eq!(conv.streaming_text, "");
        assert!(!conv.is_streaming);
        assert_eq!(conv.entries.len(), 1);
        assert!(matches!(
            &conv.entries[0],
            ConversationEntry::AssistantText { text, .. } if text == "done"
        ));
    }

    #[test]
    fn conversation_handles_tool_use_and_result() {
        let mut conv = WorkerConversation::new();
        conv.handle_event(&AgentEventWire::ToolUse {
            tool: "Bash".into(),
            input: "ls -la".into(),
        });
        assert_eq!(conv.tool_count, 1);
        conv.handle_event(&AgentEventWire::ToolResult {
            output: "file.txt".into(),
            is_error: false,
        });
        assert!(matches!(
            &conv.entries[0],
            ConversationEntry::ToolCall { tool, output: Some(out), is_error: false, .. }
            if tool == "Bash" && out == "file.txt"
        ));
    }

    #[test]
    fn conversation_handles_session_waiting() {
        let mut conv = WorkerConversation::new();
        conv.handle_event(&AgentEventWire::SessionWaiting {
            session_id: "sess-1".into(),
        });
        assert!(matches!(
            &conv.entries[0],
            ConversationEntry::Status { text } if text.contains("Waiting")
        ));
    }

    #[test]
    fn conversation_handles_error() {
        let mut conv = WorkerConversation::new();
        conv.handle_event(&AgentEventWire::Error {
            message: "something broke".into(),
        });
        assert!(matches!(
            &conv.entries[0],
            ConversationEntry::Status { text } if text.contains("something broke")
        ));
    }

    #[test]
    fn conversation_handles_session_result() {
        let mut conv = WorkerConversation::new();
        conv.handle_event(&AgentEventWire::SessionResult {
            turns: 5,
            cost_usd: Some(0.10),
            session_id: Some("sess".into()),
        });
        assert_eq!(conv.turn_count, 5);
        assert_eq!(conv.cost_usd, Some(0.10));
    }

    #[test]
    fn conversation_tool_use_flushes_streaming() {
        let mut conv = WorkerConversation::new();
        conv.handle_event(&AgentEventWire::TextDelta {
            text: "before".into(),
        });
        conv.handle_event(&AgentEventWire::ToolUse {
            tool: "Read".into(),
            input: "main.rs".into(),
        });
        assert_eq!(conv.entries.len(), 2);
        assert!(matches!(
            &conv.entries[0],
            ConversationEntry::AssistantText { text, .. } if text == "before"
        ));
        assert!(matches!(
            &conv.entries[1],
            ConversationEntry::ToolCall { tool, .. } if tool == "Read"
        ));
    }

    #[test]
    fn app_update_worker_list_clamps_selection() {
        let mut app = DaemonTuiApp::new(PathBuf::from("/tmp"));
        app.selected = 5;
        app.update_worker_list(vec![WorkerInfo {
            id: "w-1".into(),
            branch: "b".into(),
            prompt: "p".into(),
            agent: "claude".into(),
            phase: WorkerPhase::Running,
            session_id: None,
            pr_url: None,
            pr_number: None,
            pr_title: None,
            pr_state: None,
            restart_count: 0,
            created_at: None,
            agent_card: None,
            role: None,
            review_verdict: None,
        }]);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn app_select_next_prev() {
        let mut app = DaemonTuiApp::new(PathBuf::from("/tmp"));
        let workers = (0..3)
            .map(|i| WorkerInfo {
                id: format!("w-{}", i),
                branch: "b".into(),
                prompt: "p".into(),
                agent: "claude".into(),
                phase: WorkerPhase::Running,
                session_id: None,
                pr_url: None,
                pr_number: None,
                pr_title: None,
                pr_state: None,
                restart_count: 0,
                created_at: None,
                agent_card: None,
                role: None,
                review_verdict: None,
            })
            .collect();
        app.update_worker_list(workers);

        app.select_next();
        assert_eq!(app.selected, 1);
        app.select_next();
        assert_eq!(app.selected, 2);
        app.select_next();
        assert_eq!(app.selected, 2); // clamped

        app.select_prev();
        assert_eq!(app.selected, 1);
        app.select_prev();
        assert_eq!(app.selected, 0);
        app.select_prev();
        assert_eq!(app.selected, 0); // clamped
    }

    #[test]
    fn app_input_handling() {
        let mut app = DaemonTuiApp::new(PathBuf::from("/tmp"));
        app.input_char('h');
        app.input_char('i');
        assert_eq!(app.input_buffer, "hi");
        app.input_backspace();
        assert_eq!(app.input_buffer, "h");
        let taken = app.take_input();
        assert_eq!(taken, "h");
        assert_eq!(app.input_buffer, "");
    }

    #[test]
    fn scroll_operations() {
        let mut conv = WorkerConversation::new();
        conv.total_visual_lines = 100; // simulate rendered content
        conv.scroll_up(5);
        assert_eq!(conv.scroll_offset, 5);
        assert!(!conv.auto_scroll);
        conv.scroll_down(3);
        assert_eq!(conv.scroll_offset, 2);
        conv.scroll_to_bottom();
        assert_eq!(conv.scroll_offset, 0);
        assert!(conv.auto_scroll);
    }

    // ── Tool focus tests ──

    fn make_tool(name: &str) -> ConversationEntry {
        ConversationEntry::ToolCall {
            tool: name.to_string(),
            input: "input".to_string(),
            output: None,
            is_error: false,
            collapsed: true,
        }
    }

    fn make_text(text: &str) -> ConversationEntry {
        ConversationEntry::AssistantText {
            text: text.to_string(),
            timestamp: String::new(),
        }
    }

    #[test]
    fn focus_next_tool_cycles() {
        let mut conv = WorkerConversation::new();
        conv.entries.push(make_text("hello"));
        conv.entries.push(make_tool("Bash"));
        conv.entries.push(make_text("world"));
        conv.entries.push(make_tool("Read"));

        // First call: no focus -> first tool
        conv.focus_next_tool();
        assert_eq!(conv.focused_tool, Some(1));

        // Next: advance to second tool
        conv.focus_next_tool();
        assert_eq!(conv.focused_tool, Some(3));

        // Wrap around
        conv.focus_next_tool();
        assert_eq!(conv.focused_tool, Some(1));
    }

    #[test]
    fn focus_prev_tool_cycles() {
        let mut conv = WorkerConversation::new();
        conv.entries.push(make_tool("Bash"));
        conv.entries.push(make_text("hello"));
        conv.entries.push(make_tool("Read"));

        // First call: no focus -> last tool
        conv.focus_prev_tool();
        assert_eq!(conv.focused_tool, Some(2));

        // Prev: go to first tool
        conv.focus_prev_tool();
        assert_eq!(conv.focused_tool, Some(0));

        // Wrap around
        conv.focus_prev_tool();
        assert_eq!(conv.focused_tool, Some(2));
    }

    #[test]
    fn focus_no_tools() {
        let mut conv = WorkerConversation::new();
        conv.entries.push(make_text("hello"));
        conv.focus_next_tool();
        assert_eq!(conv.focused_tool, None);
        conv.focus_prev_tool();
        assert_eq!(conv.focused_tool, None);
    }

    #[test]
    fn toggle_focused_tool() {
        let mut conv = WorkerConversation::new();
        conv.entries.push(make_tool("Bash"));
        conv.focused_tool = Some(0);
        assert!(matches!(
            &conv.entries[0],
            ConversationEntry::ToolCall {
                collapsed: true,
                ..
            }
        ));
        conv.toggle_focused_tool();
        assert!(matches!(
            &conv.entries[0],
            ConversationEntry::ToolCall {
                collapsed: false,
                ..
            }
        ));
        conv.toggle_focused_tool();
        assert!(matches!(
            &conv.entries[0],
            ConversationEntry::ToolCall {
                collapsed: true,
                ..
            }
        ));
    }

    #[test]
    fn validate_focus_clears_invalid() {
        let mut conv = WorkerConversation::new();
        conv.entries.push(make_tool("Bash"));
        conv.focused_tool = Some(5); // out of bounds
        conv.validate_focus();
        assert_eq!(conv.focused_tool, None);
    }

    #[test]
    fn validate_focus_clears_non_tool() {
        let mut conv = WorkerConversation::new();
        conv.entries.push(make_text("hello"));
        conv.focused_tool = Some(0);
        conv.validate_focus();
        assert_eq!(conv.focused_tool, None);
    }

    #[test]
    fn validate_focus_keeps_valid() {
        let mut conv = WorkerConversation::new();
        conv.entries.push(make_tool("Bash"));
        conv.focused_tool = Some(0);
        conv.validate_focus();
        assert_eq!(conv.focused_tool, Some(0));
    }

    #[test]
    fn toggle_tool_at_sets_focus() {
        let mut conv = WorkerConversation::new();
        conv.entries.push(make_text("hello"));
        conv.entries.push(make_tool("Bash"));
        conv.toggle_tool_at(1);
        assert_eq!(conv.focused_tool, Some(1));
        assert!(matches!(
            &conv.entries[1],
            ConversationEntry::ToolCall {
                collapsed: false,
                ..
            }
        ));
    }

    #[test]
    fn clear_focus_works() {
        let mut conv = WorkerConversation::new();
        conv.entries.push(make_tool("Bash"));
        conv.focused_tool = Some(0);
        conv.clear_focus();
        assert_eq!(conv.focused_tool, None);
    }

    // ── PR detail tests ──

    #[test]
    fn pr_detail_from_worker_with_pr() {
        let w = WorkerInfo {
            id: "hive-1".into(),
            branch: "swarm/fix".into(),
            prompt: "fix".into(),
            agent: "claude".into(),
            phase: WorkerPhase::Waiting,
            session_id: None,
            pr_url: Some("https://github.com/ApiariTools/hive/pull/42".into()),
            pr_number: Some(42),
            pr_title: Some("Fix the thing".into()),
            pr_state: Some("OPEN".into()),
            restart_count: 0,
            created_at: None,
            agent_card: None,
            role: None,
            review_verdict: None,
        };
        let detail = PrDetailInfo::from_worker(&w).unwrap();
        assert_eq!(detail.number, 42);
        assert_eq!(detail.title, "Fix the thing");
        assert_eq!(detail.state, "OPEN");
        assert_eq!(detail.worker_id, "hive-1");
    }

    #[test]
    fn pr_detail_from_worker_no_pr() {
        let w = WorkerInfo {
            id: "hive-1".into(),
            branch: "swarm/fix".into(),
            prompt: "fix".into(),
            agent: "claude".into(),
            phase: WorkerPhase::Running,
            session_id: None,
            pr_url: None,
            pr_number: None,
            pr_title: None,
            pr_state: None,
            restart_count: 0,
            created_at: None,
            agent_card: None,
            role: None,
            review_verdict: None,
        };
        assert!(PrDetailInfo::from_worker(&w).is_none());
    }

    #[test]
    fn pr_detail_extracts_number_from_url() {
        let w = WorkerInfo {
            id: "hive-2".into(),
            branch: "swarm/feat".into(),
            prompt: "feat".into(),
            agent: "claude".into(),
            phase: WorkerPhase::Waiting,
            session_id: None,
            pr_url: Some("https://github.com/ApiariTools/hive/pull/99".into()),
            pr_number: None, // Not provided — should extract from URL
            pr_title: None,
            pr_state: None,
            restart_count: 0,
            created_at: None,
            agent_card: None,
            role: None,
            review_verdict: None,
        };
        let detail = PrDetailInfo::from_worker(&w).unwrap();
        assert_eq!(detail.number, 99);
        assert_eq!(detail.title, "PR #99"); // fallback title
        assert_eq!(detail.state, "OPEN"); // fallback state
    }

    #[test]
    fn handle_phase_change_updates_worker() {
        let mut app = DaemonTuiApp::new(PathBuf::from("/tmp"));
        app.update_worker_list(vec![WorkerInfo {
            id: "hive-1".into(),
            branch: "b".into(),
            prompt: "p".into(),
            agent: "claude-tui".into(),
            phase: WorkerPhase::Running,
            session_id: None,
            pr_url: None,
            pr_number: None,
            pr_title: None,
            pr_state: None,
            restart_count: 0,
            created_at: None,
            agent_card: None,
            role: None,
            review_verdict: None,
        }]);
        assert_eq!(app.workers[0].phase, WorkerPhase::Running);

        app.handle_phase_change("hive-1", &WorkerPhase::Waiting);
        assert_eq!(app.workers[0].phase, WorkerPhase::Waiting);
    }

    #[test]
    fn handle_agent_event_creates_conversation() {
        let mut app = DaemonTuiApp::new(PathBuf::from("/tmp"));
        assert!(app.conversations.get("hive-1").is_none());

        app.handle_agent_event(
            "hive-1",
            &AgentEventWire::TextDelta {
                text: "hello".into(),
            },
        );

        let conv = app.conversations.get("hive-1").unwrap();
        assert_eq!(conv.streaming_text, "hello");
        assert!(conv.is_streaming);
    }

    // ── History parsing tests ──

    #[test]
    fn parse_history_events_from_agent_event_format() {
        // events.jsonl stores AgentEvent (with timestamps), not AgentEventWire
        let content = r#"{"type":"assistant_text","timestamp":"2025-01-01T00:00:00Z","text":"hello world"}
{"type":"tool_use","timestamp":"2025-01-01T00:00:01Z","tool":"Bash","input":"ls -la"}
{"type":"tool_result","timestamp":"2025-01-01T00:00:02Z","tool":"Bash","output":"file.txt","is_error":false}
{"type":"session_result","timestamp":"2025-01-01T00:00:03Z","turns":5,"cost_usd":0.10,"session_id":"sess-1"}
"#;
        let entries = parse_history_events(content);
        assert_eq!(entries.len(), 4);
        assert!(
            matches!(&entries[0], HistoryEntry::Event(AgentEventWire::TextDelta { text }) if text == "hello world")
        );
        assert!(
            matches!(&entries[1], HistoryEntry::Event(AgentEventWire::ToolUse { tool, .. }) if tool == "Bash")
        );
        assert!(
            matches!(&entries[2], HistoryEntry::Event(AgentEventWire::ToolResult { output, is_error: false, .. }) if output == "file.txt")
        );
        assert!(matches!(
            &entries[3],
            HistoryEntry::Event(AgentEventWire::SessionResult { turns: 5, .. })
        ));
    }

    #[test]
    fn parse_history_events_includes_user_messages() {
        let content = r#"{"type":"assistant_text","timestamp":"2025-01-01T00:00:00Z","text":"done"}
{"type":"user_message","timestamp":"2025-01-01T00:00:01Z","text":"thanks"}
{"type":"assistant_text","timestamp":"2025-01-01T00:00:02Z","text":"welcome"}
"#;
        let entries = parse_history_events(content);
        assert_eq!(entries.len(), 3);
        assert!(matches!(&entries[1], HistoryEntry::UserMessage(text) if text == "thanks"));
    }

    #[test]
    fn parse_history_events_skips_start_markers() {
        let content = r#"{"type":"start","timestamp":"2025-01-01T00:00:00Z","prompt":"do thing","model":"claude"}
{"type":"assistant_text","timestamp":"2025-01-01T00:00:01Z","text":"ok"}
"#;
        let entries = parse_history_events(content);
        assert_eq!(entries.len(), 1);
        assert!(matches!(
            &entries[0],
            HistoryEntry::Event(AgentEventWire::TextDelta { .. })
        ));
    }

    #[test]
    fn parse_history_events_handles_errors() {
        let content = r#"{"type":"error","timestamp":"2025-01-01T00:00:00Z","message":"something broke"}
"#;
        let entries = parse_history_events(content);
        assert_eq!(entries.len(), 1);
        assert!(
            matches!(&entries[0], HistoryEntry::Event(AgentEventWire::Error { message }) if message == "something broke")
        );
    }

    /// End-to-end test: parse history → feed to handle_agent_event → verify conversation
    #[test]
    fn history_loads_into_conversation_entries() {
        let mut app = DaemonTuiApp::new(PathBuf::from("/tmp"));

        let content = r#"{"type":"start","timestamp":"2025-01-01T00:00:00Z","prompt":"do stuff","model":"claude"}
{"type":"assistant_text","timestamp":"2025-01-01T00:00:01Z","text":"I'll help you."}
{"type":"tool_use","timestamp":"2025-01-01T00:00:02Z","tool":"Read","input":"src/main.rs"}
{"type":"tool_result","timestamp":"2025-01-01T00:00:03Z","tool":"Read","output":"fn main() {}","is_error":false}
{"type":"assistant_text","timestamp":"2025-01-01T00:00:04Z","text":"Done!"}
{"type":"session_result","timestamp":"2025-01-01T00:00:05Z","turns":1,"cost_usd":0.05,"session_id":"s1"}
"#;

        let entries = parse_history_events(content);
        assert_eq!(
            entries.len(),
            5,
            "should parse 5 entries (start is skipped)"
        );

        let wt_id = "test-worker";
        for entry in &entries {
            match entry {
                HistoryEntry::Event(event) => {
                    app.handle_agent_event(wt_id, event);
                }
                HistoryEntry::UserMessage(text) => {
                    let conv = app
                        .conversations
                        .entry(wt_id.to_string())
                        .or_insert_with(WorkerConversation::new);
                    conv.entries.push(ConversationEntry::User {
                        text: text.clone(),
                        timestamp: String::new(),
                    });
                }
            }
        }

        let conv = app.conversations.get(wt_id).unwrap();
        // After SessionResult, flush_streaming_text is called, so all text should be in entries
        assert!(
            !conv.entries.is_empty(),
            "conversation should have entries after loading history"
        );
        // Should have: AssistantText("I'll help you."), ToolCall(Read), AssistantText("Done!")
        assert_eq!(
            conv.entries.len(),
            3,
            "entries: {:?}",
            conv.entries
                .iter()
                .map(|e| match e {
                    ConversationEntry::AssistantText { text, .. } =>
                        format!("Text({})", &text[..text.len().min(20)]),
                    ConversationEntry::ToolCall { tool, .. } => format!("Tool({})", tool),
                    ConversationEntry::User { text, .. } =>
                        format!("User({})", &text[..text.len().min(20)]),
                    ConversationEntry::Status { text } =>
                        format!("Status({})", &text[..text.len().min(20)]),
                    ConversationEntry::Question { text, .. } =>
                        format!("Question({})", &text[..text.len().min(20)]),
                })
                .collect::<Vec<_>>()
        );
    }
}
