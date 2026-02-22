use apiari_common::ipc::JsonlWriter;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A structured event written to the agent's event log for hive consumption.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Session started.
    Start {
        timestamp: DateTime<Utc>,
        prompt: String,
        model: Option<String>,
    },
    /// Assistant emitted text.
    AssistantText {
        timestamp: DateTime<Utc>,
        text: String,
    },
    /// Assistant requested a tool call.
    ToolUse {
        timestamp: DateTime<Utc>,
        tool: String,
        input: String,
    },
    /// Tool execution completed.
    ToolResult {
        timestamp: DateTime<Utc>,
        tool: String,
        output: String,
        is_error: bool,
    },
    /// Session completed.
    Complete {
        timestamp: DateTime<Utc>,
        turns: u64,
        cost_usd: Option<f64>,
        session_id: Option<String>,
    },
    /// Session errored.
    Error {
        timestamp: DateTime<Utc>,
        message: String,
    },
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

    /// Log session completion.
    pub fn log_complete(&self, turns: u64, cost_usd: Option<f64>, session_id: Option<&str>) {
        self.log(&AgentEvent::Complete {
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
