//! Agent status file helpers.
//!
//! The daemon writes a per-worker status file at
//! `.swarm/agent-status/<worktree_id>` so external tools (hive, keeper)
//! can detect the agent's current state without a socket connection.

use std::path::Path;

/// Directory name under `.swarm/` for agent status files.
const STATUS_DIR: &str = "agent-status";

/// Write the agent status file for a worker.
pub fn write_agent_status(work_dir: &Path, worktree_id: &str, status: &str) {
    let status_dir = work_dir.join(".swarm").join(STATUS_DIR);
    let _ = std::fs::create_dir_all(&status_dir);
    let _ = std::fs::write(status_dir.join(worktree_id), status);
}

/// Read the agent status file for a worker.
///
/// Returns `None` if the file doesn't exist or can't be read.
/// Returns `Some(status)` with the trimmed file contents otherwise.
#[allow(dead_code)]
pub fn read_agent_status(work_dir: &Path, worktree_id: &str) -> Option<String> {
    let path = work_dir
        .join(".swarm")
        .join(STATUS_DIR)
        .join(worktree_id);
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_status_none_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let result = read_agent_status(dir.path(), "nonexistent-worker");
        assert!(result.is_none());
    }

    #[test]
    fn agent_status_waiting_parsed() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_status(dir.path(), "worker-1", "waiting");
        let result = read_agent_status(dir.path(), "worker-1");
        assert_eq!(result.as_deref(), Some("waiting"));
    }

    #[test]
    fn agent_status_running_parsed() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_status(dir.path(), "worker-2", "running");
        let result = read_agent_status(dir.path(), "worker-2");
        assert_eq!(result.as_deref(), Some("running"));
    }

    #[test]
    fn agent_status_unknown_value_handled() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_status(dir.path(), "worker-3", "banana");
        // Unknown values are still returned — callers decide what to do
        let result = read_agent_status(dir.path(), "worker-3");
        assert_eq!(result.as_deref(), Some("banana"));
    }

    #[test]
    fn agent_status_empty_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_status(dir.path(), "worker-4", "");
        let result = read_agent_status(dir.path(), "worker-4");
        assert!(result.is_none());
    }

    #[test]
    fn agent_status_whitespace_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_status(dir.path(), "worker-5", "  running\n");
        let result = read_agent_status(dir.path(), "worker-5");
        assert_eq!(result.as_deref(), Some("running"));
    }

    #[test]
    fn agent_status_overwrite_updates_value() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_status(dir.path(), "worker-6", "running");
        assert_eq!(
            read_agent_status(dir.path(), "worker-6").as_deref(),
            Some("running")
        );

        write_agent_status(dir.path(), "worker-6", "waiting");
        assert_eq!(
            read_agent_status(dir.path(), "worker-6").as_deref(),
            Some("waiting")
        );
    }

    #[test]
    fn agent_status_separate_workers_isolated() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_status(dir.path(), "worker-a", "running");
        write_agent_status(dir.path(), "worker-b", "waiting");

        assert_eq!(
            read_agent_status(dir.path(), "worker-a").as_deref(),
            Some("running")
        );
        assert_eq!(
            read_agent_status(dir.path(), "worker-b").as_deref(),
            Some("waiting")
        );
    }
}
