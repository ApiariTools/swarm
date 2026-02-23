use apiari_common::ipc::{JsonlReader, JsonlWriter};
use chrono::{DateTime, Local};
use color_eyre::Result;
use serde::{Deserialize, Serialize};
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
        #[serde(default)]
        start_point: Option<String>,
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
    "claude-tui".to_string()
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
    CreateFailed {
        error: String,
        prompt: String,
        repo: Option<String>,
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
    let mut reader = JsonlReader::<InboxMessage>::with_offset(path, offset);
    let messages = reader.poll()?;
    Ok((messages, reader.offset()))
}

/// Append a message to inbox.jsonl.
pub fn write_inbox(work_dir: &Path, msg: &InboxMessage) -> Result<()> {
    let writer = JsonlWriter::<InboxMessage>::new(inbox_path(work_dir));
    writer.append(msg)?;
    Ok(())
}

/// Append an event to events.jsonl.
pub fn emit_event(work_dir: &Path, event: &SwarmEvent) -> Result<()> {
    let writer = JsonlWriter::<SwarmEvent>::new(events_path(work_dir));
    writer.append(event)?;
    Ok(())
}

// ── Per-Agent Inbox ───────────────────────────────────────

/// A message sent to a specific agent's inbox (used by claude-tui).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInboxMessage {
    pub message: String,
    pub timestamp: DateTime<Local>,
}

fn agent_inbox_path(work_dir: &Path, worktree_id: &str) -> std::path::PathBuf {
    work_dir
        .join(".swarm")
        .join("agents")
        .join(worktree_id)
        .join("inbox.jsonl")
}

/// Write a message to a specific agent's inbox.
pub fn write_agent_inbox(work_dir: &Path, worktree_id: &str, message: &str) -> Result<()> {
    let writer = JsonlWriter::<AgentInboxMessage>::new(agent_inbox_path(work_dir, worktree_id));
    writer.append(&AgentInboxMessage {
        message: message.to_string(),
        timestamp: Local::now(),
    })?;
    Ok(())
}

/// Read new messages from an agent's inbox starting at byte offset.
pub fn read_agent_inbox(
    work_dir: &Path,
    worktree_id: &str,
    offset: u64,
) -> Result<(Vec<AgentInboxMessage>, u64)> {
    let path = agent_inbox_path(work_dir, worktree_id);
    let mut reader = JsonlReader::<AgentInboxMessage>::with_offset(path, offset);
    let messages = reader.poll()?;
    Ok((messages, reader.offset()))
}


#[cfg(test)]
mod tests {
    use super::*;

    // --- InboxMessage serialization tests ---

    #[test]
    fn test_create_message_round_trips() {
        let msg = InboxMessage::Create {
            id: "msg-1".to_string(),
            prompt: "fix the login bug".to_string(),
            agent: "claude".to_string(),
            repo: Some("swarm".to_string()),
            start_point: None,
            timestamp: Local::now(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let restored: InboxMessage = serde_json::from_str(&json).unwrap();
        match restored {
            InboxMessage::Create { prompt, agent, repo, .. } => {
                assert_eq!(prompt, "fix the login bug");
                assert_eq!(agent, "claude");
                assert_eq!(repo, Some("swarm".to_string()));
            }
            _ => panic!("expected Create variant"),
        }
    }

    #[test]
    fn test_send_message_round_trips() {
        let msg = InboxMessage::Send {
            id: "msg-2".to_string(),
            worktree: "hive-1".to_string(),
            message: "please review the PR".to_string(),
            timestamp: Local::now(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let restored: InboxMessage = serde_json::from_str(&json).unwrap();
        match restored {
            InboxMessage::Send { worktree, message, .. } => {
                assert_eq!(worktree, "hive-1");
                assert_eq!(message, "please review the PR");
            }
            _ => panic!("expected Send variant"),
        }
    }

    #[test]
    fn test_close_message_round_trips() {
        let msg = InboxMessage::Close {
            id: "msg-3".to_string(),
            worktree: "hive-2".to_string(),
            timestamp: Local::now(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let restored: InboxMessage = serde_json::from_str(&json).unwrap();
        match restored {
            InboxMessage::Close { worktree, .. } => {
                assert_eq!(worktree, "hive-2");
            }
            _ => panic!("expected Close variant"),
        }
    }

    #[test]
    fn test_merge_message_round_trips() {
        let msg = InboxMessage::Merge {
            id: "msg-4".to_string(),
            worktree: "hive-3".to_string(),
            timestamp: Local::now(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let restored: InboxMessage = serde_json::from_str(&json).unwrap();
        match restored {
            InboxMessage::Merge { worktree, .. } => {
                assert_eq!(worktree, "hive-3");
            }
            _ => panic!("expected Merge variant"),
        }
    }

    #[test]
    fn test_create_message_defaults_agent_to_claude_tui() {
        // Simulate a JSON message without the "agent" field
        let json = r#"{"action":"create","id":"x","prompt":"test","timestamp":"2025-01-01T00:00:00-05:00"}"#;
        let msg: InboxMessage = serde_json::from_str(json).unwrap();
        match msg {
            InboxMessage::Create { agent, repo, start_point, .. } => {
                assert_eq!(agent, "claude-tui");
                assert!(repo.is_none());
                assert!(start_point.is_none());
            }
            _ => panic!("expected Create variant"),
        }
    }

    #[test]
    fn test_create_message_unknown_action_is_err() {
        let json = r#"{"action":"unknown","id":"x","timestamp":"2025-01-01T00:00:00-05:00"}"#;
        let result = serde_json::from_str::<InboxMessage>(json);
        assert!(result.is_err());
    }

    // --- IPC write/read cycle tests ---

    #[test]
    fn test_ipc_write_and_read_inbox() {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path();

        let msg = InboxMessage::Create {
            id: "msg-1".to_string(),
            prompt: "add tests".to_string(),
            agent: "claude".to_string(),
            repo: Some("swarm".to_string()),
            start_point: None,
            timestamp: Local::now(),
        };

        write_inbox(work_dir, &msg).unwrap();

        let (messages, new_pos) = read_inbox(work_dir, 0).unwrap();
        assert_eq!(messages.len(), 1);
        assert!(new_pos > 0);

        match &messages[0] {
            InboxMessage::Create { prompt, .. } => {
                assert_eq!(prompt, "add tests");
            }
            _ => panic!("expected Create"),
        }

        // Reading again from the new position should return nothing
        let (messages2, _) = read_inbox(work_dir, new_pos).unwrap();
        assert!(messages2.is_empty());
    }

    #[test]
    fn test_ipc_multiple_messages_sequential() {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path();

        let msg1 = InboxMessage::Create {
            id: "msg-1".to_string(),
            prompt: "task one".to_string(),
            agent: "claude".to_string(),
            repo: None,
            start_point: None,
            timestamp: Local::now(),
        };
        let msg2 = InboxMessage::Send {
            id: "msg-2".to_string(),
            worktree: "hive-1".to_string(),
            message: "hello".to_string(),
            timestamp: Local::now(),
        };
        let msg3 = InboxMessage::Close {
            id: "msg-3".to_string(),
            worktree: "hive-1".to_string(),
            timestamp: Local::now(),
        };

        write_inbox(work_dir, &msg1).unwrap();
        write_inbox(work_dir, &msg2).unwrap();

        // Read first batch
        let (messages, pos) = read_inbox(work_dir, 0).unwrap();
        assert_eq!(messages.len(), 2);

        // Write a third, then read from where we left off
        write_inbox(work_dir, &msg3).unwrap();
        let (messages2, _) = read_inbox(work_dir, pos).unwrap();
        assert_eq!(messages2.len(), 1);
        match &messages2[0] {
            InboxMessage::Close { worktree, .. } => assert_eq!(worktree, "hive-1"),
            _ => panic!("expected Close"),
        }
    }

    #[test]
    fn test_ipc_read_empty_inbox() {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path();
        // Inbox file doesn't exist yet
        let (messages, pos) = read_inbox(work_dir, 0).unwrap();
        assert!(messages.is_empty());
        assert_eq!(pos, 0);
    }

    #[test]
    fn test_ipc_send_to_unknown_worktree_no_panic() {
        // This tests that the Send message can be deserialized even when
        // targeting a worktree that doesn't exist. The actual "no-op on
        // unknown worktree" logic is in App::process_inbox, but this
        // verifies the IPC layer handles it cleanly.
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path();

        let msg = InboxMessage::Send {
            id: "msg-1".to_string(),
            worktree: "nonexistent-99".to_string(),
            message: "this goes nowhere".to_string(),
            timestamp: Local::now(),
        };

        write_inbox(work_dir, &msg).unwrap();
        let (messages, _) = read_inbox(work_dir, 0).unwrap();
        assert_eq!(messages.len(), 1);
        match &messages[0] {
            InboxMessage::Send { worktree, .. } => {
                assert_eq!(worktree, "nonexistent-99");
            }
            _ => panic!("expected Send"),
        }
    }

    // --- Event emission tests ---

    #[test]
    fn test_emit_create_failed_event() {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path();

        let event = SwarmEvent::CreateFailed {
            error: "unknown repo 'bogus'".to_string(),
            prompt: "fix something".to_string(),
            repo: Some("bogus".to_string()),
            timestamp: Local::now(),
        };

        emit_event(work_dir, &event).unwrap();

        // Verify the event was written to events.jsonl
        let events_file = work_dir.join(".swarm").join("events.jsonl");
        let content = std::fs::read_to_string(&events_file).unwrap();
        assert!(content.contains("create_failed"));
        assert!(content.contains("bogus"));
    }

    // --- Agent inbox tests ---

    #[test]
    fn test_agent_inbox_write_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path();

        write_agent_inbox(work_dir, "worker-1", "please review the PR").unwrap();
        write_agent_inbox(work_dir, "worker-1", "also update the tests").unwrap();

        let (messages, pos) = read_agent_inbox(work_dir, "worker-1", 0).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].message, "please review the PR");
        assert_eq!(messages[1].message, "also update the tests");

        // Reading again returns nothing
        let (messages2, _) = read_agent_inbox(work_dir, "worker-1", pos).unwrap();
        assert!(messages2.is_empty());
    }

    #[test]
    fn test_agent_inbox_separate_workers() {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path();

        write_agent_inbox(work_dir, "worker-1", "msg for worker 1").unwrap();
        write_agent_inbox(work_dir, "worker-2", "msg for worker 2").unwrap();

        let (msgs1, _) = read_agent_inbox(work_dir, "worker-1", 0).unwrap();
        let (msgs2, _) = read_agent_inbox(work_dir, "worker-2", 0).unwrap();

        assert_eq!(msgs1.len(), 1);
        assert_eq!(msgs1[0].message, "msg for worker 1");
        assert_eq!(msgs2.len(), 1);
        assert_eq!(msgs2[0].message, "msg for worker 2");
    }
}
