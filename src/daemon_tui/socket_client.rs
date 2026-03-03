use crate::daemon::protocol::{DaemonRequest, DaemonResponse};
use color_eyre::Result;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Transport layer: Unix socket or TCP.
enum Transport {
    Unix {
        reader: BufReader<tokio::net::unix::OwnedReadHalf>,
        writer: tokio::net::unix::OwnedWriteHalf,
    },
    Tcp {
        reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
        writer: tokio::net::tcp::OwnedWriteHalf,
    },
}

/// Async client for the swarm daemon (Unix socket or TCP).
pub struct DaemonClient {
    transport: Transport,
}

impl DaemonClient {
    /// Connect to the daemon via Unix socket.
    /// Tries per-workspace socket first, then global.
    pub async fn connect(work_dir: &Path) -> Result<Self> {
        let local = crate::core::ipc::socket_path(work_dir);
        let sock_path = if local.exists() {
            local
        } else {
            crate::core::ipc::global_socket_path()
        };
        let stream = UnixStream::connect(&sock_path).await?;
        let (read_half, write_half) = stream.into_split();
        Ok(Self {
            transport: Transport::Unix {
                reader: BufReader::new(read_half),
                writer: write_half,
            },
        })
    }

    /// Connect to the daemon via TCP.
    pub async fn connect_tcp(addr: &str) -> Result<Self> {
        let stream = tokio::net::TcpStream::connect(addr).await?;
        let (read_half, write_half) = stream.into_split();
        Ok(Self {
            transport: Transport::Tcp {
                reader: BufReader::new(read_half),
                writer: write_half,
            },
        })
    }

    /// Send a Subscribe request to enter streaming mode.
    /// Returns the initial OK response.
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
        match &mut self.transport {
            Transport::Unix { writer, .. } => {
                writer.write_all(json.as_bytes()).await?;
                writer.flush().await?;
            }
            Transport::Tcp { writer, .. } => {
                writer.write_all(json.as_bytes()).await?;
                writer.flush().await?;
            }
        }
        Ok(())
    }

    /// Read the next response/event from the daemon.
    pub async fn next_response(&mut self) -> Result<DaemonResponse> {
        let mut line = String::new();
        let n = match &mut self.transport {
            Transport::Unix { reader, .. } => reader.read_line(&mut line).await?,
            Transport::Tcp { reader, .. } => reader.read_line(&mut line).await?,
        };
        if n == 0 {
            return Err(color_eyre::eyre::eyre!("daemon disconnected"));
        }
        let resp: DaemonResponse = serde_json::from_str(line.trim())?;
        Ok(resp)
    }
}
