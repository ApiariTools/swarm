use crate::daemon::protocol::{DaemonRequest, DaemonResponse};
use color_eyre::Result;
use std::path::Path;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Reader half of a daemon connection.
///
/// Designed to be moved into a background tokio task that reads responses
/// in a tight loop and forwards them via an mpsc channel. This avoids the
/// cancellation-safety issues of using `BufReader::read_line` inside
/// `tokio::select!`.
pub struct DaemonReader {
    inner: Box<dyn AsyncBufRead + Unpin + Send>,
}

impl DaemonReader {
    /// Read one JSONL response line. Returns Err on EOF or I/O error.
    pub async fn next_response(&mut self) -> Result<DaemonResponse> {
        let mut line = String::new();
        let n = self.inner.read_line(&mut line).await?;
        if n == 0 {
            return Err(color_eyre::eyre::eyre!("daemon disconnected"));
        }
        let resp: DaemonResponse = serde_json::from_str(line.trim())?;
        Ok(resp)
    }
}

/// Async client for the swarm daemon (Unix socket or TCP).
///
/// After connecting and subscribing, call `take_reader()` to extract the
/// reader half for use in a background task. The client retains the writer
/// for sending requests.
pub struct DaemonClient {
    writer: Box<dyn AsyncWrite + Unpin + Send>,
    reader: Option<Box<dyn AsyncBufRead + Unpin + Send>>,
}

impl DaemonClient {
    /// Create a disconnected placeholder client.
    /// All sends will fail, triggering the reconnect loop.
    pub fn disconnected() -> Self {
        Self {
            writer: Box::new(tokio::io::sink()),
            reader: None,
        }
    }

    /// Connect to the daemon via Unix socket.
    /// Tries per-workspace socket first, falls back to global.
    pub async fn connect(work_dir: &Path) -> Result<Self> {
        let local = crate::core::ipc::socket_path(work_dir);
        let global = crate::core::ipc::global_socket_path();

        let stream = if local.exists() {
            match UnixStream::connect(&local).await {
                Ok(s) => s,
                Err(_) => UnixStream::connect(&global).await?,
            }
        } else {
            UnixStream::connect(&global).await?
        };
        let (read_half, write_half) = stream.into_split();
        Ok(Self {
            writer: Box::new(write_half),
            reader: Some(Box::new(BufReader::new(read_half))),
        })
    }

    /// Connect to the daemon via TCP.
    pub async fn connect_tcp(addr: &str) -> Result<Self> {
        let stream = tokio::net::TcpStream::connect(addr).await?;
        let (read_half, write_half) = stream.into_split();
        Ok(Self {
            writer: Box::new(write_half),
            reader: Some(Box::new(BufReader::new(read_half))),
        })
    }

    /// Send a Subscribe request and return the initial OK response.
    /// Must be called before `take_reader()`.
    pub async fn subscribe(&mut self, filter: Option<String>) -> Result<DaemonResponse> {
        let req = DaemonRequest::Subscribe {
            worktree_id: filter,
            workspace: None,
        };
        self.send(&req).await?;
        self.next_response().await
    }

    /// Send a request to the daemon.
    pub async fn send(&mut self, req: &DaemonRequest) -> Result<()> {
        let mut json = serde_json::to_string(req)?;
        json.push('\n');
        self.writer.write_all(json.as_bytes()).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Read the next response (only works before `take_reader()`).
    pub async fn next_response(&mut self) -> Result<DaemonResponse> {
        let reader = self
            .reader
            .as_mut()
            .ok_or_else(|| color_eyre::eyre::eyre!("reader already taken"))?;
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(color_eyre::eyre::eyre!("daemon disconnected"));
        }
        let resp: DaemonResponse = serde_json::from_str(line.trim())?;
        Ok(resp)
    }

    /// Extract the reader half for use in a background task.
    /// After this, only `send()` is available on the client.
    pub fn take_reader(&mut self) -> Option<DaemonReader> {
        self.reader.take().map(|inner| DaemonReader { inner })
    }
}
