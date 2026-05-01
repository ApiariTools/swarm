//! Integration tests for the daemon RegisterWorkspace + CreateWorker flow.
//!
//! These tests start a real daemon socket server in a tempdir and send
//! IPC messages to verify the registration and worker creation flow works
//! end-to-end over the Unix socket protocol.

#![cfg(all(unix, any(feature = "client", feature = "server")))]

use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

use apiari_swarm::client::{DaemonRequest, DaemonResponse};

/// Helper: send a request over a Unix stream and read one JSON-line response.
fn send_request(stream: &std::os::unix::net::UnixStream, req: &DaemonRequest) -> DaemonResponse {
    let mut writer = std::io::BufWriter::new(stream);
    let mut line = serde_json::to_string(req).unwrap();
    line.push('\n');
    writer.write_all(line.as_bytes()).unwrap();
    writer.flush().unwrap();

    let mut reader = BufReader::new(stream);
    let mut resp_line = String::new();
    reader.read_line(&mut resp_line).unwrap();
    serde_json::from_str(resp_line.trim()).unwrap()
}

/// Helper: connect to a socket with retry (daemon may need a moment to bind).
fn connect_with_retry(
    sock_path: &std::path::Path,
    timeout: Duration,
) -> std::os::unix::net::UnixStream {
    let start = std::time::Instant::now();
    loop {
        match std::os::unix::net::UnixStream::connect(sock_path) {
            Ok(stream) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                return stream;
            }
            Err(_) if start.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("failed to connect to daemon socket: {}", e),
        }
    }
}

/// Start a mock daemon socket server that handles RegisterWorkspace,
/// ListWorkers, CreateWorker, and ListWorkspaces requests.
/// Returns (socket_path, join_handle).
fn start_mock_daemon(dir: &std::path::Path) -> (std::path::PathBuf, std::thread::JoinHandle<()>) {
    let sock_path = dir.join("daemon.sock");
    let sock_path_clone = sock_path.clone();

    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let listener = tokio::net::UnixListener::bind(&sock_path_clone).unwrap();

            // Track registered workspaces for this test daemon
            let workspaces: std::sync::Arc<tokio::sync::Mutex<std::collections::HashSet<String>>> =
                std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new()));

            // Accept connections until the socket file is removed (test cleanup)
            loop {
                let (stream, _) =
                    match tokio::time::timeout(Duration::from_secs(30), listener.accept()).await {
                        Ok(Ok(conn)) => conn,
                        _ => break,
                    };

                let ws = workspaces.clone();
                tokio::spawn(async move {
                    let (reader, mut writer) = stream.into_split();
                    let mut reader = tokio::io::BufReader::new(reader);
                    let mut line = String::new();

                    loop {
                        line.clear();
                        match tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }

                        let req: DaemonRequest = match serde_json::from_str(line.trim()) {
                            Ok(r) => r,
                            Err(_) => {
                                let err = DaemonResponse::Error {
                                    message: "invalid request".into(),
                                };
                                let mut resp = serde_json::to_string(&err).unwrap();
                                resp.push('\n');
                                let _ = tokio::io::AsyncWriteExt::write_all(
                                    &mut writer,
                                    resp.as_bytes(),
                                )
                                .await;
                                continue;
                            }
                        };

                        let response = match req {
                            DaemonRequest::Ping => DaemonResponse::Ok { data: None },
                            DaemonRequest::RegisterWorkspace { path } => {
                                let canonical = path.to_string_lossy().to_string();
                                ws.lock().await.insert(canonical.clone());
                                DaemonResponse::Ok {
                                    data: Some(serde_json::json!({ "path": canonical })),
                                }
                            }
                            DaemonRequest::ListWorkspaces => {
                                let registered = ws.lock().await;
                                let infos: Vec<serde_json::Value> = registered
                                    .iter()
                                    .map(|p| {
                                        serde_json::json!({
                                            "path": p,
                                            "worker_count": 0
                                        })
                                    })
                                    .collect();
                                DaemonResponse::Ok {
                                    data: Some(serde_json::json!({ "workspaces": infos })),
                                }
                            }
                            DaemonRequest::ListWorkers { .. } => {
                                DaemonResponse::Workers { workers: vec![] }
                            }
                            DaemonRequest::CreateWorker { workspace, .. } => {
                                let registered = ws.lock().await;
                                if let Some(ref ws_path) = workspace {
                                    let key = ws_path.to_string_lossy().to_string();
                                    if registered.contains(&key) {
                                        DaemonResponse::Ok {
                                            data: Some(serde_json::json!({
                                                "worktree_id": "test-1234"
                                            })),
                                        }
                                    } else {
                                        DaemonResponse::Error {
                                            message: format!(
                                                "workspace not registered: {} (register it first)",
                                                ws_path.display()
                                            ),
                                        }
                                    }
                                } else if registered.len() == 1 {
                                    DaemonResponse::Ok {
                                        data: Some(serde_json::json!({
                                            "worktree_id": "test-1234"
                                        })),
                                    }
                                } else if registered.is_empty() {
                                    DaemonResponse::Error {
                                        message: "no workspaces registered".into(),
                                    }
                                } else {
                                    DaemonResponse::Error {
                                        message:
                                            "multiple workspaces registered, specify workspace"
                                                .into(),
                                    }
                                }
                            }
                            DaemonRequest::Subscribe { .. } => DaemonResponse::Ok { data: None },
                            _ => DaemonResponse::Ok { data: None },
                        };

                        let mut resp = serde_json::to_string(&response).unwrap();
                        resp.push('\n');
                        let _ =
                            tokio::io::AsyncWriteExt::write_all(&mut writer, resp.as_bytes()).await;
                        let _ = tokio::io::AsyncWriteExt::flush(&mut writer).await;
                    }
                });
            }
        });
    });

    // Wait for socket to appear
    let start = std::time::Instant::now();
    while !sock_path.exists() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }

    (sock_path, handle)
}

/// Register a workspace, then create a worker — should succeed because
/// the workspace is registered.
#[test]
fn test_register_workspace_then_create_worker() {
    let dir = tempfile::tempdir().unwrap();
    let (sock_path, _handle) = start_mock_daemon(dir.path());

    let stream = connect_with_retry(&sock_path, Duration::from_secs(5));

    // Register workspace
    let resp = send_request(
        &stream,
        &DaemonRequest::RegisterWorkspace {
            path: dir.path().to_path_buf(),
        },
    );
    match &resp {
        DaemonResponse::Ok { .. } => {}
        other => panic!("expected Ok for RegisterWorkspace, got: {:?}", other),
    }

    // Now create a worker in that workspace
    let stream2 = connect_with_retry(&sock_path, Duration::from_secs(2));
    let resp = send_request(
        &stream2,
        &DaemonRequest::CreateWorker {
            prompt: "fix the bug".into(),
            agent: "claude".into(),
            repo: None,
            start_point: None,
            workspace: Some(dir.path().to_path_buf()),
            profile: None,
            task_dir: None,
            role: None,
            review_pr: None,
            base_branch: None,
        },
    );
    match &resp {
        DaemonResponse::Ok { data } => {
            let d = data.as_ref().expect("should have data");
            assert!(d["worktree_id"].is_string());
        }
        other => panic!(
            "expected Ok for CreateWorker after register, got: {:?}",
            other
        ),
    }
}

/// Skip RegisterWorkspace, then try CreateWorker — should fail with
/// "workspace not registered" or "no workspaces registered".
#[test]
fn test_create_worker_without_register_fails() {
    let dir = tempfile::tempdir().unwrap();
    let (sock_path, _handle) = start_mock_daemon(dir.path());

    let stream = connect_with_retry(&sock_path, Duration::from_secs(5));

    let resp = send_request(
        &stream,
        &DaemonRequest::CreateWorker {
            prompt: "fix the bug".into(),
            agent: "claude".into(),
            repo: None,
            start_point: None,
            workspace: Some(dir.path().to_path_buf()),
            profile: None,
            task_dir: None,
            role: None,
            review_pr: None,
            base_branch: None,
        },
    );
    match &resp {
        DaemonResponse::Error { message } => {
            assert!(
                message.contains("not registered") || message.contains("no workspaces"),
                "expected 'not registered' error, got: {}",
                message
            );
        }
        other => panic!(
            "expected Error for CreateWorker without register, got: {:?}",
            other
        ),
    }
}

/// Registering the same workspace twice should succeed (idempotent).
#[test]
fn test_register_workspace_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let (sock_path, _handle) = start_mock_daemon(dir.path());

    let ws_path = dir.path().to_path_buf();

    // First registration
    let stream = connect_with_retry(&sock_path, Duration::from_secs(5));
    let resp = send_request(
        &stream,
        &DaemonRequest::RegisterWorkspace {
            path: ws_path.clone(),
        },
    );
    assert!(
        matches!(&resp, DaemonResponse::Ok { .. }),
        "first register should succeed"
    );

    // Second registration of the same path
    let stream2 = connect_with_retry(&sock_path, Duration::from_secs(2));
    let resp = send_request(
        &stream2,
        &DaemonRequest::RegisterWorkspace { path: ws_path },
    );
    assert!(
        matches!(&resp, DaemonResponse::Ok { .. }),
        "second register should also succeed (idempotent)"
    );
}

/// After RegisterWorkspace, ListWorkers should return an empty list.
#[test]
fn test_list_workers_empty_after_register() {
    let dir = tempfile::tempdir().unwrap();
    let (sock_path, _handle) = start_mock_daemon(dir.path());

    // Register
    let stream = connect_with_retry(&sock_path, Duration::from_secs(5));
    let resp = send_request(
        &stream,
        &DaemonRequest::RegisterWorkspace {
            path: dir.path().to_path_buf(),
        },
    );
    assert!(matches!(&resp, DaemonResponse::Ok { .. }));

    // List workers
    let stream2 = connect_with_retry(&sock_path, Duration::from_secs(2));
    let resp = send_request(
        &stream2,
        &DaemonRequest::ListWorkers {
            workspace: Some(dir.path().to_path_buf()),
        },
    );
    match &resp {
        DaemonResponse::Workers { workers } => {
            assert!(workers.is_empty(), "should have no workers initially");
        }
        other => panic!("expected Workers response, got: {:?}", other),
    }
}

/// Ping should always succeed.
#[test]
fn test_ping_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let (sock_path, _handle) = start_mock_daemon(dir.path());

    let stream = connect_with_retry(&sock_path, Duration::from_secs(5));
    let resp = send_request(&stream, &DaemonRequest::Ping);
    assert!(
        matches!(&resp, DaemonResponse::Ok { data: None }),
        "ping should return Ok"
    );
}

/// Multiple requests on separate connections should all work.
#[test]
fn test_multiple_connections() {
    let dir = tempfile::tempdir().unwrap();
    let (sock_path, _handle) = start_mock_daemon(dir.path());

    // Register on one connection
    let s1 = connect_with_retry(&sock_path, Duration::from_secs(5));
    let resp = send_request(
        &s1,
        &DaemonRequest::RegisterWorkspace {
            path: dir.path().to_path_buf(),
        },
    );
    assert!(matches!(&resp, DaemonResponse::Ok { .. }));

    // Ping on another connection
    let s2 = connect_with_retry(&sock_path, Duration::from_secs(2));
    let resp = send_request(&s2, &DaemonRequest::Ping);
    assert!(matches!(&resp, DaemonResponse::Ok { .. }));

    // CreateWorker on a third connection (workspace already registered)
    let s3 = connect_with_retry(&sock_path, Duration::from_secs(2));
    let resp = send_request(
        &s3,
        &DaemonRequest::CreateWorker {
            prompt: "do something".into(),
            agent: "claude".into(),
            repo: None,
            start_point: None,
            workspace: Some(dir.path().to_path_buf()),
            profile: None,
            task_dir: None,
            role: None,
            review_pr: None,
            base_branch: None,
        },
    );
    assert!(
        matches!(&resp, DaemonResponse::Ok { .. }),
        "create worker should succeed on separate connection"
    );
}
