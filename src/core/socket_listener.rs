use super::ipc::{self, InboxAck, InboxMessage};
use crate::swarm_log;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Handle to the background socket listener.
/// On drop, aborts the accept loop and removes the socket file.
pub struct SocketListenerHandle {
    task: JoinHandle<()>,
    socket_path: PathBuf,
}

impl Drop for SocketListenerHandle {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Start the Unix domain socket listener.
///
/// Binds to `.swarm/swarm.sock`, spawns a background task that accepts
/// connections and sends parsed `InboxMessage`s through the returned channel.
pub fn start(
    work_dir: &Path,
) -> color_eyre::Result<(mpsc::UnboundedReceiver<InboxMessage>, SocketListenerHandle)> {
    let sock_path = ipc::socket_path(work_dir);

    // Ensure .swarm/ directory exists
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Clean up stale socket from a previous crash
    ipc::cleanup_stale_socket(work_dir);

    // Bind the listener
    let listener = UnixListener::bind(&sock_path)?;

    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(accept_loop(listener, tx));

    let handle = SocketListenerHandle {
        task,
        socket_path: sock_path,
    };

    Ok((rx, handle))
}

/// Accept loop: waits for connections and spawns a handler for each.
async fn accept_loop(listener: UnixListener, tx: mpsc::UnboundedSender<InboxMessage>) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let tx = tx.clone();
                tokio::spawn(handle_connection(stream, tx));
            }
            Err(e) => {
                swarm_log!("[swarm] socket accept error: {}", e);
                // Brief pause to avoid tight error loops
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

/// Handle a single connection: read one JSON line, parse, forward, ack.
async fn handle_connection(
    stream: tokio::net::UnixStream,
    tx: mpsc::UnboundedSender<InboxMessage>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(read_half);
    let mut line = String::new();

    // Read one line (with timeout)
    let read_result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reader.read_line(&mut line),
    )
    .await;

    let ack = match read_result {
        Ok(Ok(0)) => {
            // Client closed without sending anything
            return;
        }
        Ok(Ok(_)) => match serde_json::from_str::<InboxMessage>(line.trim()) {
            Ok(msg) => {
                let _ = tx.send(msg);
                InboxAck {
                    ok: true,
                    error: None,
                }
            }
            Err(e) => InboxAck {
                ok: false,
                error: Some(format!("parse error: {}", e)),
            },
        },
        Ok(Err(e)) => InboxAck {
            ok: false,
            error: Some(format!("read error: {}", e)),
        },
        Err(_) => InboxAck {
            ok: false,
            error: Some("read timeout".to_string()),
        },
    };

    // Write ack (best effort)
    if let Ok(ack_json) = serde_json::to_string(&ack) {
        let mut ack_line = ack_json;
        ack_line.push('\n');
        let _ = write_half.write_all(ack_line.as_bytes()).await;
        let _ = write_half.flush().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;

    #[tokio::test]
    async fn test_socket_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path();
        std::fs::create_dir_all(work_dir.join(".swarm")).unwrap();

        let (mut rx, _handle) = start(work_dir).unwrap();

        // Send a message via socket
        let msg = InboxMessage::Create {
            id: "test-1".to_string(),
            prompt: "hello socket".to_string(),
            agent: "claude".to_string(),
            repo: None,
            start_point: None,
            timestamp: Local::now(),
        };

        let delivered = ipc::send_via_socket(work_dir, &msg).await.unwrap();
        assert!(delivered);

        // Receive it
        let received = rx.recv().await.unwrap();
        match received {
            InboxMessage::Create { prompt, .. } => assert_eq!(prompt, "hello socket"),
            _ => panic!("expected Create"),
        }
    }

    #[tokio::test]
    async fn test_socket_fallback_when_no_listener() {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path();
        std::fs::create_dir_all(work_dir.join(".swarm")).unwrap();

        // No listener started — send_via_socket should return false
        let msg = InboxMessage::Send {
            id: "test-2".to_string(),
            worktree: "wt-1".to_string(),
            message: "hi".to_string(),
            timestamp: Local::now(),
        };

        let delivered = ipc::send_via_socket(work_dir, &msg).await.unwrap();
        assert!(!delivered);
    }

    #[tokio::test]
    async fn test_send_inbox_uses_socket_when_available() {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path();
        std::fs::create_dir_all(work_dir.join(".swarm")).unwrap();

        let (mut rx, _handle) = start(work_dir).unwrap();

        let msg = InboxMessage::Close {
            id: "test-3".to_string(),
            worktree: "wt-2".to_string(),
            timestamp: Local::now(),
        };

        ipc::send_inbox(work_dir, &msg).await.unwrap();

        let received = rx.recv().await.unwrap();
        match received {
            InboxMessage::Close { worktree, .. } => assert_eq!(worktree, "wt-2"),
            _ => panic!("expected Close"),
        }
    }

    #[tokio::test]
    async fn test_send_inbox_falls_back_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path();
        std::fs::create_dir_all(work_dir.join(".swarm")).unwrap();

        // No listener — should fall back to JSONL
        let msg = InboxMessage::Merge {
            id: "test-4".to_string(),
            worktree: "wt-3".to_string(),
            timestamp: Local::now(),
        };

        ipc::send_inbox(work_dir, &msg).await.unwrap();

        // Verify it was written to the file
        let (messages, _) = ipc::read_inbox(work_dir, 0).unwrap();
        assert_eq!(messages.len(), 1);
        match &messages[0] {
            InboxMessage::Merge { worktree, .. } => assert_eq!(worktree, "wt-3"),
            _ => panic!("expected Merge"),
        }
    }

    #[tokio::test]
    async fn test_cleanup_stale_socket() {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path();
        let sock = work_dir.join(".swarm").join("swarm.sock");
        std::fs::create_dir_all(sock.parent().unwrap()).unwrap();

        // Use std::os::unix::net::UnixListener (not tokio) so that
        // drop() closes the file descriptor immediately without needing
        // the tokio runtime to process the close.
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        drop(listener);
        // Socket file exists but nobody is listening
        assert!(sock.exists());

        ipc::cleanup_stale_socket(work_dir);
        assert!(!sock.exists());
    }

    #[tokio::test]
    async fn test_multiple_messages_in_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path();
        std::fs::create_dir_all(work_dir.join(".swarm")).unwrap();

        let (mut rx, _handle) = start(work_dir).unwrap();

        for i in 0..5 {
            let msg = InboxMessage::Send {
                id: format!("msg-{}", i),
                worktree: "wt-1".to_string(),
                message: format!("message {}", i),
                timestamp: Local::now(),
            };
            let delivered = ipc::send_via_socket(work_dir, &msg).await.unwrap();
            assert!(delivered);
        }

        for i in 0..5 {
            let received = rx.recv().await.unwrap();
            match received {
                InboxMessage::Send { message, .. } => {
                    assert_eq!(message, format!("message {}", i));
                }
                _ => panic!("expected Send"),
            }
        }
    }
}
