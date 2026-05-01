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
///
/// # Panics
/// Panics if called outside a tokio runtime.
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
        "daemon failed to start within 5 seconds — check .swarm/swarm.log"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_process_alive_current_process() {
        let pid = std::process::id();
        assert!(super::super::is_process_alive(pid));
    }

    #[test]
    fn test_is_process_alive_dead_process() {
        // Spawn a child, wait for it to exit, then confirm is_process_alive
        // returns false for its (now-dead) PID.
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("failed to spawn 'true'");
        let pid = child.id();
        child.wait().unwrap(); // reap the zombie
        assert!(
            !super::super::is_process_alive(pid),
            "reaped child PID {} should not be alive",
            pid
        );
    }

    #[test]
    fn test_read_global_pid_matches_running_state() {
        // Verify that read_global_pid and is_daemon_running are consistent:
        // if read_global_pid returns Some(pid), then is_process_alive(pid)
        // should agree with is_daemon_running.
        let dir = tempfile::tempdir().unwrap();
        let pid = super::super::read_global_pid();
        let running = is_daemon_running(dir.path());
        match pid {
            Some(p) => assert_eq!(
                running,
                super::super::is_process_alive(p),
                "is_daemon_running should agree with is_process_alive for PID {}",
                p
            ),
            None => assert!(
                !running,
                "is_daemon_running should be false when no PID file exists"
            ),
        }
    }

    #[tokio::test]
    async fn test_ensure_daemon_running_finds_local_socket() {
        // Pre-bind a listener on the local socket path so
        // ensure_daemon_running's connect check succeeds immediately.
        // spawn_daemon will also fire (it reads the global PID file, not
        // our tempdir), but the local socket is found first.
        let dir = tempfile::tempdir().unwrap();
        let sock_dir = dir.path().join(".swarm");
        std::fs::create_dir_all(&sock_dir).unwrap();
        let sock_path = sock_dir.join("swarm.sock");

        let _listener = tokio::net::UnixListener::bind(&sock_path).unwrap();

        let result = tokio::time::timeout(
            tokio::time::Duration::from_secs(3),
            ensure_daemon_running(dir.path()),
        )
        .await
        .expect("should not timeout");

        assert!(
            result.is_ok(),
            "ensure_daemon_running should succeed when local socket is listening: {:?}",
            result.err()
        );
    }
}
