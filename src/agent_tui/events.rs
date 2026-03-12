use apiari_common::ipc::JsonlWriter;
pub use apiari_tui::conversation::ConversationEntry;
pub use apiari_tui::events_parser::AgentEvent;
use chrono::{DateTime, Local, Utc};
use std::io::BufRead;
use std::path::{Path, PathBuf};

/// Format a UTC timestamp as local time for display.
fn fmt_ts(ts: &DateTime<Utc>) -> String {
    ts.with_timezone(&Local).format("%-I:%M %p").to_string()
}

/// Writes agent events to a JSONL file for hive consumption.
pub struct EventLogger {
    writer: JsonlWriter<AgentEvent>,
}

impl EventLogger {
    /// Create a new event logger at the given path.
    pub fn new(path: PathBuf) -> Self {
        Self {
            writer: JsonlWriter::new(path),
        }
    }

    /// Log an event, silently ignoring write errors.
    pub fn log(&self, event: &AgentEvent) {
        let _ = self.writer.append(event);
    }

    /// Log a session start.
    pub fn log_start(&self, prompt: &str, model: Option<&str>) {
        self.log(&AgentEvent::Start {
            timestamp: Utc::now(),
            prompt: prompt.to_string(),
            model: model.map(String::from),
        });
    }

    /// Log a user follow-up message.
    pub fn log_user_message(&self, text: &str) {
        self.log(&AgentEvent::UserMessage {
            timestamp: Utc::now(),
            text: text.to_string(),
        });
    }

    /// Log assistant text.
    pub fn log_text(&self, text: &str) {
        self.log(&AgentEvent::AssistantText {
            timestamp: Utc::now(),
            text: text.to_string(),
        });
    }

    /// Log a tool use request.
    pub fn log_tool_use(&self, tool: &str, input: &str) {
        self.log(&AgentEvent::ToolUse {
            timestamp: Utc::now(),
            tool: tool.to_string(),
            input: input.to_string(),
        });
    }

    /// Log a tool result.
    pub fn log_tool_result(&self, tool: &str, output: &str, is_error: bool) {
        self.log(&AgentEvent::ToolResult {
            timestamp: Utc::now(),
            tool: tool.to_string(),
            output: output.to_string(),
            is_error,
        });
    }

    /// Log a session result (SDK returned a result, session is now idle/resumable).
    pub fn log_session_result(&self, turns: u64, cost_usd: Option<f64>, session_id: Option<&str>) {
        self.log(&AgentEvent::SessionResult {
            timestamp: Utc::now(),
            turns,
            cost_usd,
            session_id: session_id.map(String::from),
        });
    }

    /// Log an error.
    pub fn log_error(&self, message: &str) {
        self.log(&AgentEvent::Error {
            timestamp: Utc::now(),
            message: message.to_string(),
        });
    }
}

/// A restored previous session from events.jsonl.
pub struct PreviousSession {
    pub session_id: String,
    pub entries: Vec<ConversationEntry>,
    pub turns: u32,
    pub cost_usd: Option<f64>,
    pub tool_count: u32,
    pub model: Option<String>,
}

/// Read the last completed session from an events.jsonl file.
///
/// Finds the last `Start` event, replays events from that point into
/// `ConversationEntry` items, and returns `Some` only if a `Complete` event
/// with a non-None `session_id` was found.
pub fn read_last_session(path: &Path) -> Option<PreviousSession> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);

    let mut events: Vec<AgentEvent> = Vec::new();
    let mut last_start_idx: Option<usize> = None;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue, // Skip I/O errors (partial writes, invalid UTF-8)
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<AgentEvent>(&line) {
            if matches!(event, AgentEvent::Start { .. }) {
                last_start_idx = Some(events.len());
            }
            events.push(event);
        }
    }

    let start_idx = last_start_idx?;
    let session_events = &events[start_idx..];

    // Extract metadata from session events
    let mut session_id = None;
    let mut turns: u32 = 0;
    let mut cost_usd = None;
    let mut model = None;
    let mut tool_count: u32 = 0;

    for ev in session_events {
        match ev {
            AgentEvent::Start { model: m, .. } => {
                model = m.clone();
            }
            AgentEvent::SessionResult {
                session_id: Some(sid),
                turns: t,
                cost_usd: c,
                ..
            } => {
                session_id = Some(sid.clone());
                turns = *t as u32;
                cost_usd = *c;
            }
            AgentEvent::ToolUse { .. } => {
                tool_count += 1;
            }
            _ => {}
        }
    }

    let session_id = session_id?;

    // Replay events into ConversationEntry
    let mut entries: Vec<ConversationEntry> = Vec::new();

    for ev in session_events {
        match ev {
            AgentEvent::Start {
                prompt, timestamp, ..
            } => {
                entries.push(ConversationEntry::User {
                    text: prompt.clone(),
                    timestamp: fmt_ts(timestamp),
                });
            }
            AgentEvent::UserMessage {
                text, timestamp, ..
            } => {
                entries.push(ConversationEntry::User {
                    text: text.clone(),
                    timestamp: fmt_ts(timestamp),
                });
            }
            AgentEvent::AssistantText {
                text, timestamp, ..
            } => {
                entries.push(ConversationEntry::AssistantText {
                    text: text.clone(),
                    timestamp: fmt_ts(timestamp),
                });
            }
            AgentEvent::ToolUse { tool, input, .. } => {
                entries.push(ConversationEntry::ToolCall {
                    tool: tool.clone(),
                    input: input.clone(),
                    output: None,
                    is_error: false,
                    collapsed: true,
                });
            }
            AgentEvent::ToolResult {
                output, is_error, ..
            } => {
                // Update the last ToolCall entry
                if let Some(ConversationEntry::ToolCall {
                    output: o,
                    is_error: e,
                    ..
                }) = entries.last_mut()
                {
                    *o = Some(output.clone());
                    *e = *is_error;
                }
            }
            AgentEvent::Error { message, .. } => {
                entries.push(ConversationEntry::Status {
                    text: format!("Error: {}", message),
                });
            }
            AgentEvent::SessionResult { .. } => {}
        }
    }

    Some(PreviousSession {
        session_id,
        entries,
        turns,
        cost_usd,
        tool_count,
        model,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// Helper: write raw lines to a temp file and return the path.
    fn write_events_file(lines: &[&str]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(f, "{}", line).unwrap();
        }
        f.flush().unwrap();
        f
    }

    /// Helper: serialize an AgentEvent to a JSON string.
    fn to_json(event: &AgentEvent) -> String {
        serde_json::to_string(event).unwrap()
    }

    fn ts() -> DateTime<Utc> {
        Utc::now()
    }

    // ── read_last_session ──

    #[test]
    fn restore_basic_session() {
        let events = [
            to_json(&AgentEvent::Start {
                timestamp: ts(),
                prompt: "do something".into(),
                model: Some("opus".into()),
            }),
            to_json(&AgentEvent::AssistantText {
                timestamp: ts(),
                text: "on it".into(),
            }),
            to_json(&AgentEvent::ToolUse {
                timestamp: ts(),
                tool: "Bash".into(),
                input: "ls".into(),
            }),
            to_json(&AgentEvent::ToolResult {
                timestamp: ts(),
                tool: "Bash".into(),
                output: "file.txt".into(),
                is_error: false,
            }),
            to_json(&AgentEvent::SessionResult {
                timestamp: ts(),
                turns: 3,
                cost_usd: Some(0.05),
                session_id: Some("sess-123".into()),
            }),
        ];
        let lines: Vec<&str> = events.iter().map(|s| s.as_str()).collect();
        let f = write_events_file(&lines);

        let prev = read_last_session(f.path()).unwrap();
        assert_eq!(prev.session_id, "sess-123");
        assert_eq!(prev.turns, 3);
        assert_eq!(prev.cost_usd, Some(0.05));
        assert_eq!(prev.tool_count, 1);
        assert_eq!(prev.model, Some("opus".into()));

        // entries: User, AssistantText, ToolCall (with output filled in-place by ToolResult)
        assert_eq!(prev.entries.len(), 3);
        assert!(
            matches!(&prev.entries[0], ConversationEntry::User { text, .. } if text == "do something")
        );
        assert!(
            matches!(&prev.entries[1], ConversationEntry::AssistantText { text, .. } if text == "on it")
        );
        assert!(matches!(
            &prev.entries[2],
            ConversationEntry::ToolCall { tool, output: Some(out), is_error: false, .. }
            if tool == "Bash" && out == "file.txt"
        ));
    }

    #[test]
    fn restore_none_when_no_file() {
        let result = read_last_session(Path::new("/nonexistent/events.jsonl"));
        assert!(result.is_none());
    }

    #[test]
    fn restore_none_when_empty_file() {
        let f = write_events_file(&[]);
        assert!(read_last_session(f.path()).is_none());
    }

    #[test]
    fn restore_none_when_no_session_result() {
        let events = [to_json(&AgentEvent::Start {
            timestamp: ts(),
            prompt: "hello".into(),
            model: None,
        })];
        let lines: Vec<&str> = events.iter().map(|s| s.as_str()).collect();
        let f = write_events_file(&lines);

        assert!(read_last_session(f.path()).is_none());
    }

    #[test]
    fn restore_none_when_session_result_has_no_session_id() {
        let events = [
            to_json(&AgentEvent::Start {
                timestamp: ts(),
                prompt: "hello".into(),
                model: None,
            }),
            to_json(&AgentEvent::SessionResult {
                timestamp: ts(),
                turns: 1,
                cost_usd: None,
                session_id: None,
            }),
        ];
        let lines: Vec<&str> = events.iter().map(|s| s.as_str()).collect();
        let f = write_events_file(&lines);

        assert!(read_last_session(f.path()).is_none());
    }

    #[test]
    fn restore_picks_last_start_event() {
        // Two sessions in the same file — should restore from the second
        let events = [
            // First session
            to_json(&AgentEvent::Start {
                timestamp: ts(),
                prompt: "first task".into(),
                model: Some("sonnet".into()),
            }),
            to_json(&AgentEvent::SessionResult {
                timestamp: ts(),
                turns: 2,
                cost_usd: Some(0.01),
                session_id: Some("old-sess".into()),
            }),
            // Second session
            to_json(&AgentEvent::Start {
                timestamp: ts(),
                prompt: "second task".into(),
                model: Some("opus".into()),
            }),
            to_json(&AgentEvent::AssistantText {
                timestamp: ts(),
                text: "working on second".into(),
            }),
            to_json(&AgentEvent::SessionResult {
                timestamp: ts(),
                turns: 5,
                cost_usd: Some(0.10),
                session_id: Some("new-sess".into()),
            }),
        ];
        let lines: Vec<&str> = events.iter().map(|s| s.as_str()).collect();
        let f = write_events_file(&lines);

        let prev = read_last_session(f.path()).unwrap();
        assert_eq!(prev.session_id, "new-sess");
        assert_eq!(prev.turns, 5);
        assert_eq!(prev.model, Some("opus".into()));
        // Entries should only contain the second session
        assert_eq!(prev.entries.len(), 2);
        assert!(
            matches!(&prev.entries[0], ConversationEntry::User { text, .. } if text == "second task")
        );
        assert!(
            matches!(&prev.entries[1], ConversationEntry::AssistantText { text, .. } if text == "working on second")
        );
    }

    #[test]
    fn restore_skips_corrupt_lines() {
        let events = [
            to_json(&AgentEvent::Start {
                timestamp: ts(),
                prompt: "test".into(),
                model: None,
            }),
            "this is not json".to_string(),
            "".to_string(),
            "{\"bad\": \"schema\"}".to_string(),
            to_json(&AgentEvent::AssistantText {
                timestamp: ts(),
                text: "reply".into(),
            }),
            to_json(&AgentEvent::SessionResult {
                timestamp: ts(),
                turns: 1,
                cost_usd: None,
                session_id: Some("sess-ok".into()),
            }),
        ];
        let lines: Vec<&str> = events.iter().map(|s| s.as_str()).collect();
        let f = write_events_file(&lines);

        let prev = read_last_session(f.path()).unwrap();
        assert_eq!(prev.session_id, "sess-ok");
        assert_eq!(prev.entries.len(), 2); // User + AssistantText (corrupt lines skipped)
    }

    #[test]
    fn restore_includes_user_followup_messages() {
        let events = [
            to_json(&AgentEvent::Start {
                timestamp: ts(),
                prompt: "initial".into(),
                model: None,
            }),
            to_json(&AgentEvent::AssistantText {
                timestamp: ts(),
                text: "done".into(),
            }),
            to_json(&AgentEvent::SessionResult {
                timestamp: ts(),
                turns: 1,
                cost_usd: None,
                session_id: Some("s1".into()),
            }),
            to_json(&AgentEvent::UserMessage {
                timestamp: ts(),
                text: "follow up question".into(),
            }),
            to_json(&AgentEvent::AssistantText {
                timestamp: ts(),
                text: "follow up answer".into(),
            }),
            to_json(&AgentEvent::SessionResult {
                timestamp: ts(),
                turns: 3,
                cost_usd: Some(0.02),
                session_id: Some("s1".into()),
            }),
        ];
        let lines: Vec<&str> = events.iter().map(|s| s.as_str()).collect();
        let f = write_events_file(&lines);

        let prev = read_last_session(f.path()).unwrap();
        assert_eq!(prev.turns, 3);
        assert_eq!(prev.entries.len(), 4);
        assert!(
            matches!(&prev.entries[0], ConversationEntry::User { text, .. } if text == "initial")
        );
        assert!(
            matches!(&prev.entries[1], ConversationEntry::AssistantText { text, .. } if text == "done")
        );
        assert!(
            matches!(&prev.entries[2], ConversationEntry::User { text, .. } if text == "follow up question")
        );
        assert!(
            matches!(&prev.entries[3], ConversationEntry::AssistantText { text, .. } if text == "follow up answer")
        );
    }

    #[test]
    fn restore_error_events_become_status_entries() {
        let events = [
            to_json(&AgentEvent::Start {
                timestamp: ts(),
                prompt: "go".into(),
                model: None,
            }),
            to_json(&AgentEvent::Error {
                timestamp: ts(),
                message: "rate limited".into(),
            }),
            to_json(&AgentEvent::SessionResult {
                timestamp: ts(),
                turns: 0,
                cost_usd: None,
                session_id: Some("s".into()),
            }),
        ];
        let lines: Vec<&str> = events.iter().map(|s| s.as_str()).collect();
        let f = write_events_file(&lines);

        let prev = read_last_session(f.path()).unwrap();
        assert!(matches!(
            &prev.entries[1],
            ConversationEntry::Status { text } if text == "Error: rate limited"
        ));
    }

    #[test]
    fn restore_counts_multiple_tools() {
        let events = [
            to_json(&AgentEvent::Start {
                timestamp: ts(),
                prompt: "go".into(),
                model: None,
            }),
            to_json(&AgentEvent::ToolUse {
                timestamp: ts(),
                tool: "Read".into(),
                input: "f.rs".into(),
            }),
            to_json(&AgentEvent::ToolResult {
                timestamp: ts(),
                tool: "Read".into(),
                output: "contents".into(),
                is_error: false,
            }),
            to_json(&AgentEvent::ToolUse {
                timestamp: ts(),
                tool: "Edit".into(),
                input: "f.rs".into(),
            }),
            to_json(&AgentEvent::ToolResult {
                timestamp: ts(),
                tool: "Edit".into(),
                output: "ok".into(),
                is_error: false,
            }),
            to_json(&AgentEvent::ToolUse {
                timestamp: ts(),
                tool: "Bash".into(),
                input: "cargo test".into(),
            }),
            to_json(&AgentEvent::ToolResult {
                timestamp: ts(),
                tool: "Bash".into(),
                output: "passed".into(),
                is_error: false,
            }),
            to_json(&AgentEvent::SessionResult {
                timestamp: ts(),
                turns: 2,
                cost_usd: None,
                session_id: Some("s".into()),
            }),
        ];
        let lines: Vec<&str> = events.iter().map(|s| s.as_str()).collect();
        let f = write_events_file(&lines);

        let prev = read_last_session(f.path()).unwrap();
        assert_eq!(prev.tool_count, 3);
    }

    #[test]
    fn restore_tool_result_updates_last_tool_call() {
        let events = [
            to_json(&AgentEvent::Start {
                timestamp: ts(),
                prompt: "go".into(),
                model: None,
            }),
            to_json(&AgentEvent::ToolUse {
                timestamp: ts(),
                tool: "Bash".into(),
                input: "exit 1".into(),
            }),
            to_json(&AgentEvent::ToolResult {
                timestamp: ts(),
                tool: "Bash".into(),
                output: "command failed".into(),
                is_error: true,
            }),
            to_json(&AgentEvent::SessionResult {
                timestamp: ts(),
                turns: 1,
                cost_usd: None,
                session_id: Some("s".into()),
            }),
        ];
        let lines: Vec<&str> = events.iter().map(|s| s.as_str()).collect();
        let f = write_events_file(&lines);

        let prev = read_last_session(f.path()).unwrap();
        assert!(matches!(
            &prev.entries[1],
            ConversationEntry::ToolCall { output: Some(out), is_error: true, collapsed: true, .. }
            if out == "command failed"
        ));
    }

    #[test]
    fn restore_last_session_result_wins() {
        // Multiple SessionResult events — last one should be used
        let events = [
            to_json(&AgentEvent::Start {
                timestamp: ts(),
                prompt: "go".into(),
                model: None,
            }),
            to_json(&AgentEvent::SessionResult {
                timestamp: ts(),
                turns: 2,
                cost_usd: Some(0.01),
                session_id: Some("first".into()),
            }),
            to_json(&AgentEvent::SessionResult {
                timestamp: ts(),
                turns: 5,
                cost_usd: Some(0.10),
                session_id: Some("last".into()),
            }),
        ];
        let lines: Vec<&str> = events.iter().map(|s| s.as_str()).collect();
        let f = write_events_file(&lines);

        let prev = read_last_session(f.path()).unwrap();
        assert_eq!(prev.session_id, "last");
        assert_eq!(prev.turns, 5);
        assert_eq!(prev.cost_usd, Some(0.10));
    }

    #[test]
    fn restore_none_when_last_session_errored_without_result() {
        // First session completed, second started but errored without SessionResult
        let events = [
            to_json(&AgentEvent::Start {
                timestamp: ts(),
                prompt: "first".into(),
                model: None,
            }),
            to_json(&AgentEvent::SessionResult {
                timestamp: ts(),
                turns: 1,
                cost_usd: None,
                session_id: Some("ok".into()),
            }),
            to_json(&AgentEvent::Start {
                timestamp: ts(),
                prompt: "second".into(),
                model: None,
            }),
            to_json(&AgentEvent::Error {
                timestamp: ts(),
                message: "crashed".into(),
            }),
        ];
        let lines: Vec<&str> = events.iter().map(|s| s.as_str()).collect();
        let f = write_events_file(&lines);

        // Last Start is "second" which has no SessionResult — should return None
        assert!(read_last_session(f.path()).is_none());
    }

    // ── EventLogger round-trip ──

    #[test]
    fn logger_writes_and_read_last_session_restores() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");

        let logger = EventLogger::new(path.clone());
        logger.log_start("build the thing", Some("opus-4"));
        logger.log_text("I'll build it");
        logger.log_tool_use("Bash", "cargo build");
        logger.log_tool_result("Bash", "ok", false);
        logger.log_session_result(4, Some(0.08), Some("round-trip-sess"));

        let prev = read_last_session(&path).unwrap();
        assert_eq!(prev.session_id, "round-trip-sess");
        assert_eq!(prev.turns, 4);
        assert_eq!(prev.cost_usd, Some(0.08));
        assert_eq!(prev.tool_count, 1);
        assert_eq!(prev.model, Some("opus-4".into()));
        assert_eq!(prev.entries.len(), 3); // User, AssistantText, ToolCall
    }

    #[test]
    fn logger_user_message_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");

        let logger = EventLogger::new(path.clone());
        logger.log_start("initial", None);
        logger.log_text("done");
        logger.log_session_result(1, None, Some("s1"));
        logger.log_user_message("followup");
        logger.log_text("followup answer");
        logger.log_session_result(2, None, Some("s1"));

        let prev = read_last_session(&path).unwrap();
        assert_eq!(prev.entries.len(), 4);
        assert!(
            matches!(&prev.entries[2], ConversationEntry::User { text, .. } if text == "followup")
        );
    }
}
