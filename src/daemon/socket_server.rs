use super::protocol::{DaemonRequest, DaemonResponse};
use crate::core::ipc::{self, InboxMessage};
use std::path::PathBuf;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

/// Handle to the daemon socket server.
/// On drop, aborts the accept loop(s) and removes the socket file.
pub struct DaemonSocketHandle {
    unix_task: JoinHandle<()>,
    tcp_task: Option<JoinHandle<()>>,
    socket_path: PathBuf,
    token_path: Option<PathBuf>,
}

impl Drop for DaemonSocketHandle {
    fn drop(&mut self) {
        self.unix_task.abort();
        if let Some(ref task) = self.tcp_task {
            task.abort();
        }
        let _ = std::fs::remove_file(&self.socket_path);
        if let Some(ref tp) = self.token_path {
            let _ = std::fs::remove_file(tp);
        }
    }
}

/// Start the daemon socket server.
///
/// Accepts connections on the global socket `~/.config/swarm/swarm.sock` (Unix)
/// and optionally on a TCP address.
/// TCP connections must authenticate with `Auth { token }` before any other request.
#[allow(clippy::type_complexity)]
pub fn start(
    event_tx: broadcast::Sender<DaemonResponse>,
    tcp_bind: Option<String>,
    auth_token: Option<String>,
) -> color_eyre::Result<(
    mpsc::UnboundedReceiver<(DaemonRequest, mpsc::UnboundedSender<DaemonResponse>)>,
    DaemonSocketHandle,
)> {
    let sock_path = ipc::global_socket_path();

    // Ensure config directory exists
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Clean up stale socket
    ipc::cleanup_stale_socket_at(&sock_path);

    let listener = UnixListener::bind(&sock_path)?;

    let (tx, rx) = mpsc::unbounded_channel();

    // Unix accept loop
    let unix_task = tokio::spawn(unix_accept_loop(listener, tx.clone(), event_tx.clone()));

    // TCP accept loop (optional)
    let mut tcp_task = None;
    let mut token_path = None;

    if let Some(ref bind_addr) = tcp_bind {
        let tcp_listener = std::net::TcpListener::bind(bind_addr)
            .map_err(|e| color_eyre::eyre::eyre!("failed to bind TCP on {}: {}", bind_addr, e))?;
        tcp_listener.set_nonblocking(true)?;
        let tcp_listener = tokio::net::TcpListener::from_std(tcp_listener)?;

        let token = auth_token.unwrap_or_default();
        if !token.is_empty() {
            // Write token file
            let tp = ipc::global_config_dir().join("daemon.token");
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                let mut opts = std::fs::OpenOptions::new();
                opts.write(true).create(true).truncate(true).mode(0o600);
                let mut f = opts.open(&tp)?;
                std::io::Write::write_all(&mut f, token.as_bytes())?;
            }
            #[cfg(not(unix))]
            {
                std::fs::write(&tp, &token)?;
            }
            tracing::info!(token = %token, "TCP auth token");
            token_path = Some(tp);
        }

        tcp_task = Some(tokio::spawn(tcp_accept_loop(
            tcp_listener,
            tx.clone(),
            event_tx,
            token.clone(),
        )));
        tracing::info!(addr = %bind_addr, "TCP listener started");
    }

    let handle = DaemonSocketHandle {
        unix_task,
        tcp_task,
        socket_path: sock_path,
        token_path,
    };

    Ok((rx, handle))
}

// ── Unix accept loop ─────────────────────────────────────────

async fn unix_accept_loop(
    listener: UnixListener,
    request_tx: mpsc::UnboundedSender<(DaemonRequest, mpsc::UnboundedSender<DaemonResponse>)>,
    event_tx: broadcast::Sender<DaemonResponse>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let request_tx = request_tx.clone();
                let event_rx = event_tx.subscribe();
                let (read_half, write_half) = stream.into_split();
                let reader = tokio::io::BufReader::new(read_half);
                tokio::spawn(handle_connection(
                    Box::new(reader),
                    Box::new(write_half),
                    request_tx,
                    event_rx,
                ));
            }
            Err(e) => {
                tracing::error!(error = %e, "Unix accept error");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

// ── TCP accept loop ──────────────────────────────────────────

async fn tcp_accept_loop(
    listener: tokio::net::TcpListener,
    request_tx: mpsc::UnboundedSender<(DaemonRequest, mpsc::UnboundedSender<DaemonResponse>)>,
    event_tx: broadcast::Sender<DaemonResponse>,
    expected_token: String,
) {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                tracing::debug!(addr = %addr, "TCP connection");
                let request_tx = request_tx.clone();
                let event_rx = event_tx.subscribe();
                let expected = expected_token.clone();
                let (read_half, write_half) = stream.into_split();
                let mut reader = tokio::io::BufReader::new(read_half);
                let mut writer: Box<dyn AsyncWrite + Unpin + Send> = Box::new(write_half);

                tokio::spawn(async move {
                    // Authenticate first if token is set
                    if !expected.is_empty() {
                        match authenticate(&mut reader, &mut writer, &expected).await {
                            Ok(true) => {}
                            Ok(false) => return,
                            Err(_) => return,
                        }
                    }
                    handle_connection(Box::new(reader), writer, request_tx, event_rx).await;
                });
            }
            Err(e) => {
                tracing::error!(error = %e, "TCP accept error");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

/// Read the first message from a TCP connection and validate the auth token.
/// Returns Ok(true) if authenticated, Ok(false) if rejected.
async fn authenticate(
    reader: &mut (impl AsyncBufRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    expected_token: &str,
) -> Result<bool, ()> {
    let mut line = String::new();
    let read_result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reader.read_line(&mut line),
    )
    .await;

    match read_result {
        Ok(Ok(0)) => Ok(false),
        Ok(Ok(_)) => {
            if let Ok(DaemonRequest::Auth { token }) =
                serde_json::from_str::<DaemonRequest>(line.trim())
                && token == expected_token
            {
                let resp = DaemonResponse::Ok { data: None };
                write_response_dyn(writer, &resp).await?;
                return Ok(true);
            }
            let resp = DaemonResponse::Error {
                message: "authentication failed".into(),
            };
            let _ = write_response_dyn(writer, &resp).await;
            Ok(false)
        }
        _ => Ok(false),
    }
}

// ── Shared connection handler ────────────────────────────────

async fn handle_connection(
    mut reader: Box<dyn AsyncBufRead + Unpin + Send>,
    mut writer: Box<dyn AsyncWrite + Unpin + Send>,
    request_tx: mpsc::UnboundedSender<(DaemonRequest, mpsc::UnboundedSender<DaemonResponse>)>,
    event_rx: broadcast::Receiver<DaemonResponse>,
) {
    let mut line = String::new();

    // Read the first request line
    let read_result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reader.read_line(&mut line),
    )
    .await;

    let request = match read_result {
        Ok(Ok(0)) => return,
        Ok(Ok(_)) => match parse_request(line.trim()) {
            Ok(req) => req,
            Err(e) => {
                let resp = DaemonResponse::Error {
                    message: format!("parse error: {}", e),
                };
                let _ = write_response_dyn(&mut writer, &resp).await;
                return;
            }
        },
        Ok(Err(e)) => {
            let resp = DaemonResponse::Error {
                message: format!("read error: {}", e),
            };
            let _ = write_response_dyn(&mut writer, &resp).await;
            return;
        }
        Err(_) => {
            let resp = DaemonResponse::Error {
                message: "read timeout".into(),
            };
            let _ = write_response_dyn(&mut writer, &resp).await;
            return;
        }
    };

    let is_subscribe = matches!(&request, DaemonRequest::Subscribe { .. });

    let (resp_tx, mut resp_rx) = mpsc::unbounded_channel();
    if request_tx.send((request, resp_tx.clone())).is_err() {
        let resp = DaemonResponse::Error {
            message: "daemon shutting down".into(),
        };
        let _ = write_response_dyn(&mut writer, &resp).await;
        return;
    }

    if let Some(resp) = resp_rx.recv().await
        && write_response_dyn(&mut writer, &resp).await.is_err()
    {
        return;
    }

    if !is_subscribe {
        return;
    }

    handle_subscription_dyn(reader, writer, request_tx, event_rx, resp_rx).await;
}

/// Handle subscription mode with trait-object I/O.
async fn handle_subscription_dyn(
    mut reader: Box<dyn AsyncBufRead + Unpin + Send>,
    mut writer: Box<dyn AsyncWrite + Unpin + Send>,
    request_tx: mpsc::UnboundedSender<(DaemonRequest, mpsc::UnboundedSender<DaemonResponse>)>,
    mut event_rx: broadcast::Receiver<DaemonResponse>,
    mut resp_rx: mpsc::UnboundedReceiver<DaemonResponse>,
) {
    let mut line = String::new();

    loop {
        tokio::select! {
            event = event_rx.recv() => {
                match event {
                    Ok(resp) => {
                        if write_response_dyn(&mut writer, &resp).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(dropped = n, "Subscriber lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return;
                    }
                }
            }

            read_result = reader.read_line(&mut line) => {
                match read_result {
                    Ok(0) => return,
                    Ok(_) => {
                        if let Ok(req) = parse_request(line.trim()) {
                            let (resp_tx, mut inner_rx) = mpsc::unbounded_channel();
                            let _ = request_tx.send((req, resp_tx));
                            if let Some(resp) = inner_rx.recv().await
                                && write_response_dyn(&mut writer, &resp).await.is_err()
                            {
                                return;
                            }
                        }
                        line.clear();
                    }
                    Err(_) => return,
                }
            }

            resp = resp_rx.recv() => {
                match resp {
                    Some(resp) => {
                        if write_response_dyn(&mut writer, &resp).await.is_err() {
                            return;
                        }
                    }
                    None => return,
                }
            }
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────

fn parse_request(line: &str) -> Result<DaemonRequest, String> {
    if let Ok(req) = serde_json::from_str::<DaemonRequest>(line) {
        return Ok(req);
    }

    if let Ok(msg) = serde_json::from_str::<InboxMessage>(line) {
        return Ok(super::protocol::translate_inbox_message(&msg));
    }

    Err(format!(
        "could not parse as DaemonRequest or InboxMessage: {}",
        &line[..line.len().min(200)]
    ))
}

/// Write a JSON response line using trait-object writer.
async fn write_response_dyn(
    writer: &mut (impl AsyncWrite + Unpin),
    resp: &DaemonResponse,
) -> Result<(), ()> {
    let mut json = serde_json::to_string(resp).map_err(|_| ())?;
    json.push('\n');
    writer.write_all(json.as_bytes()).await.map_err(|_| ())?;
    writer.flush().await.map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;

    #[test]
    fn parse_daemon_request() {
        let json = r#"{"action":"list_workers"}"#;
        let req = parse_request(json).unwrap();
        assert!(matches!(req, DaemonRequest::ListWorkers { .. }));
    }

    #[test]
    fn parse_legacy_inbox_message() {
        let msg = InboxMessage::Create {
            id: "x".into(),
            prompt: "test".into(),
            agent: "claude".into(),
            repo: None,
            start_point: None,
            timestamp: Local::now(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let req = parse_request(&json).unwrap();
        match req {
            DaemonRequest::CreateWorker { prompt, .. } => assert_eq!(prompt, "test"),
            _ => panic!("expected CreateWorker"),
        }
    }

    #[test]
    fn parse_invalid_json() {
        let result = parse_request("not json");
        assert!(result.is_err());
    }

    #[test]
    fn parse_send_message_request() {
        let json = r#"{"action":"send_message","worktree_id":"hive-1","message":"hello"}"#;
        let req = parse_request(json).unwrap();
        match req {
            DaemonRequest::SendMessage {
                worktree_id,
                message,
            } => {
                assert_eq!(worktree_id, "hive-1");
                assert_eq!(message, "hello");
            }
            _ => panic!("expected SendMessage"),
        }
    }

    #[test]
    fn parse_ping_request() {
        let json = r#"{"action":"ping"}"#;
        let req = parse_request(json).unwrap();
        assert!(matches!(req, DaemonRequest::Ping));
    }

    #[test]
    fn parse_auth_request() {
        let json = r#"{"action":"auth","token":"secret-123"}"#;
        let req = parse_request(json).unwrap();
        match req {
            DaemonRequest::Auth { token } => assert_eq!(token, "secret-123"),
            _ => panic!("expected Auth"),
        }
    }
}
