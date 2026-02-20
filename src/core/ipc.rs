use chrono::{DateTime, Local};
use color_eyre::Result;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::Path;

/// Inbox message — external commands sent to the sidebar.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum InboxMessage {
    Create {
        id: String,
        prompt: String,
        #[serde(default = "default_agent")]
        agent: String,
        #[serde(default)]
        repo: Option<String>,
        timestamp: DateTime<Local>,
    },
    Send {
        id: String,
        worktree: String,
        message: String,
        timestamp: DateTime<Local>,
    },
    Close {
        id: String,
        worktree: String,
        timestamp: DateTime<Local>,
    },
    Merge {
        id: String,
        worktree: String,
        timestamp: DateTime<Local>,
    },
}

fn default_agent() -> String {
    "claude".to_string()
}

/// Events emitted by the sidebar for external consumers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SwarmEvent {
    WorktreeCreated {
        worktree: String,
        branch: String,
        agent: String,
        pane_id: String,
        timestamp: DateTime<Local>,
    },
    AgentStarted {
        worktree: String,
        pane_id: String,
        timestamp: DateTime<Local>,
    },
    AgentDone {
        worktree: String,
        timestamp: DateTime<Local>,
    },
    WorktreeClosed {
        worktree: String,
        timestamp: DateTime<Local>,
    },
    WorktreeMerged {
        worktree: String,
        branch: String,
        timestamp: DateTime<Local>,
    },
}

fn inbox_path(work_dir: &Path) -> std::path::PathBuf {
    work_dir.join(".swarm").join("inbox.jsonl")
}

fn events_path(work_dir: &Path) -> std::path::PathBuf {
    work_dir.join(".swarm").join("events.jsonl")
}

/// Read new messages from inbox.jsonl starting at byte offset.
/// Returns parsed messages and the new offset for next read.
pub fn read_inbox(work_dir: &Path, offset: u64) -> Result<(Vec<InboxMessage>, u64)> {
    let path = inbox_path(work_dir);
    if !path.exists() {
        return Ok((Vec::new(), offset));
    }

    let file = fs::File::open(&path)?;
    let metadata = file.metadata()?;
    let file_len = metadata.len();

    if file_len <= offset {
        return Ok((Vec::new(), offset));
    }

    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(offset))?;

    let mut messages = Vec::new();
    let mut new_offset = offset;

    let mut line = String::new();
    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            break;
        }
        new_offset += bytes_read as u64;

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        match serde_json::from_str::<InboxMessage>(trimmed) {
            Ok(msg) => messages.push(msg),
            Err(_) => {} // skip malformed lines
        }
    }

    Ok((messages, new_offset))
}

/// Append a message to inbox.jsonl.
pub fn write_inbox(work_dir: &Path, msg: &InboxMessage) -> Result<()> {
    let path = inbox_path(work_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;

    let json = serde_json::to_string(msg)?;
    writeln!(file, "{}", json)?;
    Ok(())
}

/// Append an event to events.jsonl.
pub fn emit_event(work_dir: &Path, event: &SwarmEvent) -> Result<()> {
    let path = events_path(work_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;

    let json = serde_json::to_string(event)?;
    writeln!(file, "{}", json)?;
    Ok(())
}
