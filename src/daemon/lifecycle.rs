//! Daemon lifecycle helpers: check, spawn, and ensure the daemon is running.
//!
//! These were originally in `main.rs` and are now exposed through the library
//! so that external consumers (e.g. `hive`) can manage the daemon process
//! without shelling out to the `swarm` binary.

use color_eyre::Result;
use std::path::Path;

/// Check if the swarm daemon is running (global daemon).
pub fn is_daemon_running(_work_dir: &Path) -> bool {
    super::read_global_pid().is_some_and(super::is_process_alive)
}

/// Spawn the daemon process in the background.
///
/// Launches the `swarm daemon start --foreground` command as a detached child
/// process, redirecting stderr to `.swarm/daemon-stderr.log`.
pub fn spawn_daemon(work_dir: &Path) -> Result<()> {
    eprintln!("[swarm] Starting daemon...");
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "swarm".to_string());

    let log_dir = work_dir.join(".swarm");
    std::fs::create_dir_all(&log_dir).ok();
    let daemon_log = std::fs::File::create(log_dir.join("daemon-stderr.log"))
        .unwrap_or_else(|_| std::fs::File::open("/dev/null").unwrap());

    std::process::Command::new(&exe)
        .args([
            "-d",
            &work_dir.to_string_lossy(),
            "daemon",
            "start",
            "--foreground",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(daemon_log))
        .spawn()
        .map_err(|e| color_eyre::eyre::eyre!("failed to spawn daemon: {}", e))?;

    Ok(())
}

/// Ensure the daemon is running, starting it if necessary.
///
/// Waits for the daemon socket to accept connections before returning
/// (up to 5 seconds).
pub async fn ensure_daemon_running(work_dir: &Path) -> Result<()> {
    if is_daemon_running(work_dir) {
        return Ok(());
    }

    spawn_daemon(work_dir)?;

    // Wait for the daemon socket to become available (up to 5 seconds).
    let local_socket = crate::core::ipc::socket_path(work_dir);
    let global_socket = crate::core::ipc::global_socket_path();
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if tokio::net::UnixStream::connect(&local_socket).await.is_ok()
            || tokio::net::UnixStream::connect(&global_socket)
                .await
                .is_ok()
        {
            return Ok(());
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

    Err(color_eyre::eyre::eyre!(
        "daemon failed to start within 5 seconds — check .swarm/daemon-stderr.log"
    ))
}
