use crate::core::review::{ReviewConfig, deserialize_review_configs};
use crate::core::state::WorkerPhase;
use crate::swarm_log;
use apiari_common::ipc::{JsonlReader, JsonlWriter};
use chrono::{DateTime, Local};
use color_eyre::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

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
        #[serde(
            default,
            deserialize_with = "deserialize_review_configs",
            alias = "review_config"
        )]
        review_configs: Option<Vec<ReviewConfig>>,
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
    Review {
        id: String,
        worktree: String,
        #[serde(default)]
        slug: Option<String>,
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
    ReviewStarted {
        worktree: String,
        parent: String,
        slug: String,
        timestamp: DateTime<Local>,
    },
}

/// A single transition log entry (human-readable audit trail).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionEntry {
    pub worktree: String,
    pub from: WorkerPhase,
    pub to: WorkerPhase,
    pub timestamp: DateTime<Local>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

fn transitions_path(work_dir: &Path) -> std::path::PathBuf {
    work_dir.join(".swarm").join("transitions.jsonl")
}

/// Append a transition entry to `.swarm/transitions.jsonl`.
pub fn log_transition(work_dir: &Path, entry: &TransitionEntry) -> Result<()> {
    let writer = JsonlWriter::<TransitionEntry>::new(transitions_path(work_dir));
    writer.append(entry)?;
    Ok(())
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

// ── Unix Domain Socket IPC ────────────────────────────────

/// Global config directory for the swarm daemon.
pub fn global_config_dir() -> std::path::PathBuf {
    dirs::config_dir()
        .expect("no config dir")
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

/// Acknowledgment sent by the socket server after processing a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxAck {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Try sending a message via the Unix domain socket.
///
/// Returns:
/// - `Ok(true)` — message delivered and acknowledged
/// - `Ok(false)` — socket unavailable (caller should fall back to JSONL)
/// - `Err(...)` — real error (connection succeeded but something broke)
pub async fn send_via_socket(work_dir: &Path, msg: &InboxMessage) -> Result<bool> {
    let sock = socket_path(work_dir);
    if !sock.exists() {
        return Ok(false);
    }

    let stream = match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::net::UnixStream::connect(&sock),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(_)) | Err(_) => return Ok(false), // connect failed or timed out
    };

    let (read_half, mut write_half) = stream.into_split();

    // Send JSON line + shutdown write half
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.shutdown().await?;

    // Read ack with timeout
    let mut reader = tokio::io::BufReader::new(read_half);
    let mut ack_line = String::new();
    match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        reader.read_line(&mut ack_line),
    )
    .await
    {
        Ok(Ok(0)) => {
            // Server closed without ack — treat as delivered (best effort)
            Ok(true)
        }
        Ok(Ok(_)) => {
            let ack: InboxAck = serde_json::from_str(ack_line.trim())?;
            if ack.ok {
                Ok(true)
            } else {
                Err(color_eyre::eyre::eyre!(
                    "server nack: {}",
                    ack.error.unwrap_or_default()
                ))
            }
        }
        Ok(Err(e)) => Err(e.into()),
        Err(_) => {
            // Timeout reading ack — assume delivered
            Ok(true)
        }
    }
}

/// Send an inbox message: try socket first, fall back to JSONL file.
pub async fn send_inbox(work_dir: &Path, msg: &InboxMessage) -> Result<()> {
    match send_via_socket(work_dir, msg).await {
        Ok(true) => Ok(()),
        Ok(false) => {
            // Socket unavailable — fall back to file
            write_inbox(work_dir, msg)
        }
        Err(e) => {
            // Socket error — log and fall back to file
            swarm_log!("[swarm] socket send failed, falling back to file: {}", e);
            write_inbox(work_dir, msg)
        }
    }
}

/// Send a DaemonRequest to a specific socket path and return the response.
fn send_daemon_request_to(
    sock: &Path,
    req: &crate::daemon::protocol::DaemonRequest,
) -> Result<crate::daemon::protocol::DaemonResponse> {
    let stream = std::os::unix::net::UnixStream::connect(sock).map_err(|e| {
        color_eyre::eyre::eyre!("failed to connect to daemon socket: {}", e)
    })?;

    // Set read/write timeout
    let timeout = std::time::Duration::from_secs(30);
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let mut writer = std::io::BufWriter::new(&stream);
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    std::io::Write::write_all(&mut writer, line.as_bytes())?;
    std::io::Write::flush(&mut writer)?;

    let mut reader = std::io::BufReader::new(&stream);
    let mut resp_line = String::new();
    std::io::BufRead::read_line(&mut reader, &mut resp_line)?;

    let resp: crate::daemon::protocol::DaemonResponse =
        serde_json::from_str(resp_line.trim())?;
    Ok(resp)
}

/// Send a DaemonRequest to the daemon socket and return the response.
/// Tries per-workspace socket first (for test isolation), then global.
pub fn send_daemon_request(
    work_dir: &Path,
    req: &crate::daemon::protocol::DaemonRequest,
) -> Result<crate::daemon::protocol::DaemonResponse> {
    let local = socket_path(work_dir);
    if local.exists() {
        if let Ok(resp) = send_daemon_request_to(&local, req) {
            return Ok(resp);
        }
    }
    // Fall back to global socket
    send_daemon_request_to(&global_socket_path(), req)
}

/// Remove a stale socket file (left over from a crashed daemon).
/// Tries to connect; if it fails, the socket is stale and gets removed.
pub fn cleanup_stale_socket(work_dir: &Path) {
    cleanup_stale_socket_at(&socket_path(work_dir));
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
            review_configs: None,
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
    fn test_create_message_defaults_agent_to_claude_tui() {
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
            review_configs: None,
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
            review_configs: None,
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

    // ── PhaseChanged / PrDetected / TransitionEntry tests ──

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

    #[test]
    fn test_transition_entry_round_trips() {
        let entry = TransitionEntry {
            worktree: "hive-1".to_string(),
            from: WorkerPhase::Creating,
            to: WorkerPhase::Starting,
            timestamp: Local::now(),
            reason: Some("pane created".to_string()),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let restored: TransitionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.worktree, "hive-1");
        assert_eq!(restored.from, WorkerPhase::Creating);
        assert_eq!(restored.to, WorkerPhase::Starting);
        assert_eq!(restored.reason.as_deref(), Some("pane created"));
    }

    #[test]
    fn test_transition_entry_without_reason() {
        let entry = TransitionEntry {
            worktree: "hive-1".to_string(),
            from: WorkerPhase::Running,
            to: WorkerPhase::Waiting,
            timestamp: Local::now(),
            reason: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        // reason should be skipped in serialization
        assert!(!json.contains("reason"));

        let restored: TransitionEntry = serde_json::from_str(&json).unwrap();
        assert!(restored.reason.is_none());
    }

    #[test]
    fn test_log_transition_writes_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path();

        let entry = TransitionEntry {
            worktree: "hive-1".to_string(),
            from: WorkerPhase::Creating,
            to: WorkerPhase::Starting,
            timestamp: Local::now(),
            reason: Some("test".to_string()),
        };

        log_transition(work_dir, &entry).unwrap();

        let path = work_dir.join(".swarm").join("transitions.jsonl");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("creating"));
        assert!(content.contains("starting"));
        assert!(content.contains("hive-1"));
    }

    // ── Review message tests ──────────────────────────────

    #[test]
    fn test_review_message_round_trips() {
        let msg = InboxMessage::Review {
            id: "msg-5".to_string(),
            worktree: "hive-3".to_string(),
            slug: Some("code-review".to_string()),
            timestamp: Local::now(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let restored: InboxMessage = serde_json::from_str(&json).unwrap();
        match restored {
            InboxMessage::Review {
                worktree, slug, ..
            } => {
                assert_eq!(worktree, "hive-3");
                assert_eq!(slug.as_deref(), Some("code-review"));
            }
            _ => panic!("expected Review variant"),
        }
    }

    #[test]
    fn test_review_message_without_slug() {
        let msg = InboxMessage::Review {
            id: "msg-6".to_string(),
            worktree: "hive-4".to_string(),
            slug: None,
            timestamp: Local::now(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let restored: InboxMessage = serde_json::from_str(&json).unwrap();
        match restored {
            InboxMessage::Review { slug, .. } => {
                assert!(slug.is_none());
            }
            _ => panic!("expected Review variant"),
        }
    }

    #[test]
    fn test_review_started_event_round_trips() {
        let event = SwarmEvent::ReviewStarted {
            worktree: "hive-3-review-code-review".to_string(),
            parent: "hive-3".to_string(),
            slug: "code-review".to_string(),
            timestamp: Local::now(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"review_started\""));

        let restored: SwarmEvent = serde_json::from_str(&json).unwrap();
        match restored {
            SwarmEvent::ReviewStarted {
                worktree,
                parent,
                slug,
                ..
            } => {
                assert_eq!(worktree, "hive-3-review-code-review");
                assert_eq!(parent, "hive-3");
                assert_eq!(slug, "code-review");
            }
            _ => panic!("expected ReviewStarted"),
        }
    }
}
