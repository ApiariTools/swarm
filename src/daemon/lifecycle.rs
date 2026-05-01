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

/// Spawn the daemon as an in-process background task.
///
/// Launches `run_daemon` on a detached tokio task so the daemon runs within
/// the current process. This avoids shelling out to `current_exe()`, which
/// breaks when an external binary (e.g. `hive`) embeds apiari-swarm as a library.
pub fn spawn_daemon(work_dir: &Path) {
    tracing::info!("Starting daemon...");
    let work_dir = work_dir.to_path_buf();
    tokio::spawn(async move {
        if let Err(e) = super::run_daemon(Some(work_dir), None, None).await {
            tracing::error!(error = %e, "Daemon task exited with error");
        }
    });
}

/// Ensure the daemon is running, starting it if necessary.
///
/// Waits for the daemon socket to accept connections before returning
/// (up to 5 seconds).
pub async fn ensure_daemon_running(work_dir: &Path) -> Result<()> {
    if is_daemon_running(work_dir) {
        return Ok(());
    }

    spawn_daemon(work_dir);

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
        "daemon failed to start within 5 seconds — check logs"
    ))
}
