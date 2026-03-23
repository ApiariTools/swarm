use crate::core::ipc::{global_socket_path, socket_path};
use crate::daemon::protocol::{DaemonRequest, DaemonResponse};
use color_eyre::Result;
use std::path::Path;

/// Send a DaemonRequest to a specific socket path and return the response.
fn send_daemon_request_to(sock: &Path, req: &DaemonRequest) -> Result<DaemonResponse> {
    let stream = std::os::unix::net::UnixStream::connect(sock)
        .map_err(|e| color_eyre::eyre::eyre!("failed to connect to daemon socket: {}", e))?;

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

    let resp: DaemonResponse = serde_json::from_str(resp_line.trim())?;
    Ok(resp)
}

/// Send a DaemonRequest to the daemon socket and return the response.
/// Tries per-workspace socket first (for test isolation), then global.
pub fn send_daemon_request(work_dir: &Path, req: &DaemonRequest) -> Result<DaemonResponse> {
    let local = socket_path(work_dir);
    if local.exists()
        && let Ok(resp) = send_daemon_request_to(&local, req)
    {
        return Ok(resp);
    }
    // Fall back to global socket
    send_daemon_request_to(&global_socket_path(), req)
}
