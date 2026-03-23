use crate::core::state::WorkerPhase;
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
    CreateFailed {
        error: String,
        prompt: String,
        repo: Option<String>,
        timestamp: DateTime<Local>,
    },
    PhaseChanged {
        worktree: String,
        from: WorkerPhase,
        to: WorkerPhase,
        timestamp: DateTime<Local>,
    },
    PrDetected {
        worktree: String,
        pr_url: String,
        pr_title: String,
        pr_number: u64,
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

/// Append an event to events.jsonl.
pub fn emit_event(work_dir: &Path, event: &SwarmEvent) -> Result<()> {
    let writer = JsonlWriter::<SwarmEvent>::new(events_path(work_dir));
    writer.append(event)?;
    Ok(())
}

// ── Unix Domain Socket IPC ────────────────────────────────

/// Global config directory for the swarm daemon.
pub fn global_config_dir() -> std::path::PathBuf {
    dirs::home_dir()
        .expect("no home dir")
        .join(".config")
        .join("swarm")
}

/// Path to the global swarm socket file.
pub fn global_socket_path() -> std::path::PathBuf {
    global_config_dir().join("swarm.sock")
}

/// Path to the global daemon PID file.
pub fn global_pid_path() -> std::path::PathBuf {
    global_config_dir().join("daemon.pid")
}

/// Path to the per-workspace swarm socket file (legacy).
pub fn socket_path(work_dir: &Path) -> std::path::PathBuf {
    work_dir.join(".swarm").join("swarm.sock")
}

/// Remove a stale socket file at the given path.
pub fn cleanup_stale_socket_at(sock: &Path) {
    if !sock.exists() {
        return;
    }

    // Try a blocking connect to see if anyone is listening
    match std::os::unix::net::UnixStream::connect(sock) {
        Ok(_) => {
            // Someone is listening — not stale
        }
        Err(_) => {
            // No one listening — remove stale socket
            let _ = std::fs::remove_file(sock);
        }
    }
}

// ── Per-Agent Inbox ───────────────────────────────────────

/// A message sent to a specific agent's inbox.
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
            InboxMessage::Create {
                prompt,
                agent,
                repo,
                ..
            } => {
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
            InboxMessage::Send {
                worktree, message, ..
            } => {
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
    fn test_create_message_defaults_agent_to_claude() {
        // Simulate a JSON message without the "agent" field
        let json = r#"{"action":"create","id":"x","prompt":"test","timestamp":"2025-01-01T00:00:00-05:00"}"#;
        let msg: InboxMessage = serde_json::from_str(json).unwrap();
        match msg {
            InboxMessage::Create {
                agent,
                repo,
                start_point,
                ..
            } => {
                assert_eq!(agent, "claude");
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

    // ── PhaseChanged / PrDetected tests ──

    #[test]
    fn test_phase_changed_event_round_trips() {
        let event = SwarmEvent::PhaseChanged {
            worktree: "hive-1".to_string(),
            from: WorkerPhase::Starting,
            to: WorkerPhase::Running,
            timestamp: Local::now(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"phase_changed\""));
        assert!(json.contains("\"from\":\"starting\""));
        assert!(json.contains("\"to\":\"running\""));

        let restored: SwarmEvent = serde_json::from_str(&json).unwrap();
        match restored {
            SwarmEvent::PhaseChanged {
                worktree, from, to, ..
            } => {
                assert_eq!(worktree, "hive-1");
                assert_eq!(from, WorkerPhase::Starting);
                assert_eq!(to, WorkerPhase::Running);
            }
            _ => panic!("expected PhaseChanged"),
        }
    }

    #[test]
    fn test_pr_detected_event_round_trips() {
        let event = SwarmEvent::PrDetected {
            worktree: "hive-2".to_string(),
            pr_url: "https://github.com/org/repo/pull/42".to_string(),
            pr_title: "Fix auth".to_string(),
            pr_number: 42,
            timestamp: Local::now(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"pr_detected\""));

        let restored: SwarmEvent = serde_json::from_str(&json).unwrap();
        match restored {
            SwarmEvent::PrDetected {
                worktree,
                pr_url,
                pr_number,
                ..
            } => {
                assert_eq!(worktree, "hive-2");
                assert_eq!(pr_url, "https://github.com/org/repo/pull/42");
                assert_eq!(pr_number, 42);
            }
            _ => panic!("expected PrDetected"),
        }
    }
}
