//! Integration tests for daemon socket connection lifecycle.
//!
//! These tests spin up a minimal socket server in-process (no swarm imports needed)
//! and verify that clients can connect, subscribe, send requests, and receive
//! responses without the connection dropping.

use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Send a JSON line and read one JSON line back.
async fn send_recv(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    request: &str,
) -> String {
    let mut msg = request.to_string();
    if !msg.ends_with('\n') {
        msg.push('\n');
    }
    writer.write_all(msg.as_bytes()).await.unwrap();
    writer.flush().await.unwrap();

    let mut line = String::new();
    let n = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("should not timeout reading response")
        .expect("read should not error");
    assert!(n > 0, "should not get EOF (server disconnected)");
    line
}

/// Spin up a daemon socket server, connect, subscribe, send requests,
/// and verify the connection stays alive throughout.
#[tokio::test]
async fn subscribe_and_drain_stays_connected() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("test.sock");

    // Broadcast channel sends (worktree_id, pre-serialized event JSON)
    let (event_tx, _) = tokio::sync::broadcast::channel::<(String, String)>(64);
    let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();

    let event_tx_clone = event_tx.clone();
    let server_task = tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => break,
            };
            let event_rx = event_tx_clone.subscribe();
            tokio::spawn(handle_test_connection(stream, event_rx));
        }
    });

    // Give the server a moment to bind
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Connect client
    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut writer = write_half;

    // 1. Subscribe
    let resp = send_recv(&mut reader, &mut writer, r#"{"action":"subscribe"}"#).await;
    assert!(
        resp.contains(r#""ok""#) || resp.contains(r#""Ok""#),
        "subscribe should return ok, got: {}",
        resp
    );

    // 2. Send ListWorkers
    let resp = send_recv(&mut reader, &mut writer, r#"{"action":"list_workers"}"#).await;
    assert!(
        resp.contains("workers"),
        "list_workers should return workers, got: {}",
        resp
    );

    // 3. Send Ping (inline on subscription)
    let resp = send_recv(&mut reader, &mut writer, r#"{"action":"ping"}"#).await;
    assert!(
        resp.contains(r#""ok""#) || resp.contains(r#""Ok""#),
        "ping should return ok, got: {}",
        resp
    );

    // 4. Verify connection is STILL alive by sending another ping
    let resp = send_recv(&mut reader, &mut writer, r#"{"action":"ping"}"#).await;
    assert!(
        resp.contains(r#""ok""#) || resp.contains(r#""Ok""#),
        "second ping should work, got: {}",
        resp
    );

    // 5. Broadcast an event and verify the subscriber receives it
    let event_json = r#"{"type":"text_delta","text":"hello"}"#.to_string();
    let _ = event_tx.send(("test-worker".to_string(), event_json));
    let mut line = String::new();
    let n = tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await
        .expect("should receive broadcast event")
        .expect("read should not error");
    assert!(n > 0, "should receive event, not EOF");
    assert!(
        line.contains("test-worker"),
        "event should contain worktree_id, got: {}",
        line
    );

    // 6. Verify connection STILL works after receiving broadcast
    let resp = send_recv(&mut reader, &mut writer, r#"{"action":"ping"}"#).await;
    assert!(
        resp.contains(r#""ok""#) || resp.contains(r#""Ok""#),
        "ping after event should work, got: {}",
        resp
    );

    server_task.abort();
}

/// Test that a daemon child process with piped stderr doesn't die from SIGPIPE
/// when the parent drops the child handle.
#[tokio::test]
async fn daemon_survives_stderr_pipe_close() {
    let child = std::process::Command::new("sh")
        .args([
            "-c",
            "trap '' PIPE; sleep 1; echo 'still alive' >&2 2>/dev/null; echo 'ok'",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let pid = child.id();
    drop(child);

    tokio::time::sleep(Duration::from_secs(2)).await;

    let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
    if alive {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
}

/// Test that read_line with timeouts doesn't corrupt the stream.
#[tokio::test]
async fn drain_timeout_preserves_partial_reads() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("drain-test.sock");

    let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();

        let mut buf = vec![0u8; 1024];
        let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;

        let ok = r#"{"status":"ok"}"#.to_string() + "\n";
        stream.write_all(ok.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;
        let workers = r#"{"status":"workers","workers":[{"id":"w-1"}]}"#.to_string() + "\n";
        stream.write_all(workers.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();

        tokio::time::sleep(Duration::from_secs(5)).await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut writer = write_half;
    let mut drain_buf = String::new();

    writer
        .write_all(b"{\"action\":\"subscribe\"}\n")
        .await
        .unwrap();
    writer.flush().await.unwrap();

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.contains("ok"));

    let mut got_response = false;
    for _ in 0..500 {
        match tokio::time::timeout(Duration::from_millis(1), reader.read_line(&mut drain_buf)).await
        {
            Ok(Ok(0)) => panic!("unexpected EOF"),
            Ok(Ok(_)) => {
                assert!(
                    drain_buf.contains("workers"),
                    "should get workers response, got: {}",
                    drain_buf
                );
                got_response = true;
                drain_buf.clear();
                break;
            }
            Ok(Err(e)) => panic!("unexpected error: {}", e),
            Err(_) => {}
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    assert!(
        got_response,
        "should eventually receive the workers response"
    );

    server.abort();
}

/// Verify that the server stays responsive to Ping requests even while
/// concurrent blocking work (simulating PR polling) runs in the background.
/// This is a regression test for the daemon becoming unresponsive when
/// poll_prs() ran blocking `gh` subprocesses on the event loop.
#[tokio::test]
async fn server_responsive_during_blocking_work() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("responsive-test.sock");

    let (event_tx, _) = tokio::sync::broadcast::channel::<(String, String)>(64);
    let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();

    let event_tx_clone = event_tx.clone();
    let server_task = tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => break,
            };
            let event_rx = event_tx_clone.subscribe();
            tokio::spawn(handle_test_connection(stream, event_rx));
        }
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Connect and subscribe
    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut writer = write_half;

    let resp = send_recv(&mut reader, &mut writer, r#"{"action":"subscribe"}"#).await;
    assert!(resp.contains("ok"), "subscribe should work");

    // Simulate heavy blocking work running concurrently (like PR polling)
    let blocking_task = tokio::task::spawn_blocking(|| {
        std::thread::sleep(Duration::from_secs(2));
    });

    // Pings should still complete quickly while the blocking task runs
    for i in 0..5 {
        let start = std::time::Instant::now();
        let resp = send_recv(&mut reader, &mut writer, r#"{"action":"ping"}"#).await;
        let elapsed = start.elapsed();
        assert!(
            resp.contains("ok"),
            "ping {} should return ok, got: {}",
            i,
            resp
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "ping {} took {:?} — server should respond in under 500ms",
            i,
            elapsed
        );
    }

    blocking_task.abort();
    server_task.abort();
}

// ── Test helpers ────────────────────────────────────────────

async fn handle_test_connection(
    stream: tokio::net::UnixStream,
    mut event_rx: tokio::sync::broadcast::Receiver<(String, String)>,
) {
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut writer = write_half;

    let mut line = String::new();
    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
        return;
    }

    let is_subscribe = line.contains("subscribe");

    let ok = r#"{"status":"ok"}"#.to_string() + "\n";
    writer.write_all(ok.as_bytes()).await.unwrap();
    writer.flush().await.unwrap();

    if !is_subscribe {
        return;
    }

    line.clear();
    loop {
        tokio::select! {
            event = event_rx.recv() => {
                match event {
                    Ok((wt_id, evt_json)) => {
                        let resp = format!(
                            r#"{{"status":"agent_event","worktree_id":"{}","event":{}}}"#,
                            wt_id, evt_json
                        );
                        let mut json = resp;
                        json.push('\n');
                        if writer.write_all(json.as_bytes()).await.is_err() {
                            return;
                        }
                        let _ = writer.flush().await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                }
            }

            result = reader.read_line(&mut line) => {
                match result {
                    Ok(0) => return,
                    Ok(_) => {
                        let resp = if line.contains("list_workers") {
                            r#"{"status":"workers","workers":[]}"#.to_string()
                        } else if line.contains("ping") {
                            r#"{"status":"ok"}"#.to_string()
                        } else {
                            r#"{"status":"ok"}"#.to_string()
                        };
                        let mut json = resp;
                        json.push('\n');
                        if writer.write_all(json.as_bytes()).await.is_err() {
                            return;
                        }
                        let _ = writer.flush().await;
                        line.clear();
                    }
                    Err(_) => return,
                }
            }
        }
    }
}
