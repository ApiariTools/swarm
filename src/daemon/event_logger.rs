//! Standalone event logger for the daemon's agent supervisor.
//!
//! This module defines `AgentEvent` and `EventLogger` without depending on
//! `apiari-tui`, so the daemon can be compiled without TUI dependencies.
//! The serialization format is wire-compatible with `apiari_tui::events_parser::AgentEvent`.

use apiari_common::ipc::JsonlWriter;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A structured event written to the agent's event log.
///
/// Wire-compatible with `apiari_tui::events_parser::AgentEvent`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    Start {
        timestamp: DateTime<Utc>,
        prompt: String,
        model: Option<String>,
    },
    UserMessage {
        timestamp: DateTime<Utc>,
        text: String,
    },
    AssistantText {
        timestamp: DateTime<Utc>,
        text: String,
    },
    ToolUse {
        timestamp: DateTime<Utc>,
        tool: String,
        input: String,
    },
    ToolResult {
        timestamp: DateTime<Utc>,
        tool: String,
        output: String,
        is_error: bool,
    },
    SessionResult {
        timestamp: DateTime<Utc>,
        turns: u64,
        cost_usd: Option<f64>,
        session_id: Option<String>,
    },
    Error {
        timestamp: DateTime<Utc>,
        message: String,
    },
}

/// Writes agent events to a JSONL file.
pub struct EventLogger {
    writer: JsonlWriter<AgentEvent>,
}

impl EventLogger {
    pub fn new(path: PathBuf) -> Self {
        Self {
            writer: JsonlWriter::new(path),
        }
    }

    pub fn log(&self, event: &AgentEvent) {
        let _ = self.writer.append(event);
    }

    pub fn log_start(&self, prompt: &str, model: Option<&str>) {
        self.log(&AgentEvent::Start {
            timestamp: Utc::now(),
            prompt: prompt.to_string(),
            model: model.map(String::from),
        });
    }

    pub fn log_user_message(&self, text: &str) {
        self.log(&AgentEvent::UserMessage {
            timestamp: Utc::now(),
            text: text.to_string(),
        });
    }

    pub fn log_text(&self, text: &str) {
        self.log(&AgentEvent::AssistantText {
            timestamp: Utc::now(),
            text: text.to_string(),
        });
    }

    pub fn log_tool_use(&self, tool: &str, input: &str) {
        self.log(&AgentEvent::ToolUse {
            timestamp: Utc::now(),
            tool: tool.to_string(),
            input: input.to_string(),
        });
    }

    pub fn log_tool_result(&self, tool: &str, output: &str, is_error: bool) {
        self.log(&AgentEvent::ToolResult {
            timestamp: Utc::now(),
            tool: tool.to_string(),
            output: output.to_string(),
            is_error,
        });
    }

    pub fn log_session_result(&self, turns: u64, cost_usd: Option<f64>, session_id: Option<&str>) {
        self.log(&AgentEvent::SessionResult {
            timestamp: Utc::now(),
            turns,
            cost_usd,
            session_id: session_id.map(String::from),
        });
    }

    pub fn log_error(&self, message: &str) {
        self.log(&AgentEvent::Error {
            timestamp: Utc::now(),
            message: message.to_string(),
        });
    }
}
