//! Agent status file helpers.
//!
//! Status is persisted as a plain text file at `.swarm/agent-status/<worktree_id>`.
//! Hive reads these files to detect when a worker is waiting for input.

use std::path::Path;

/// Directory under the workspace root where agent status files live.
fn status_dir(work_dir: &Path) -> std::path::PathBuf {
    work_dir.join(".swarm").join("agent-status")
}

/// Write the agent status file for a given worktree.
pub fn write_agent_status(work_dir: &Path, worktree_id: &str, status: &str) {
    let dir = status_dir(work_dir);
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join(worktree_id), status);
}

/// Read the agent status file for a given worktree.
/// Returns `None` if the file does not exist or cannot be read.
pub fn read_agent_status(work_dir: &Path, worktree_id: &str) -> Option<String> {
    let path = status_dir(work_dir).join(worktree_id);
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_status_none_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let result = read_agent_status(dir.path(), "nonexistent-worker");
        assert!(result.is_none());
    }

    #[test]
    fn test_agent_status_write_and_read() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_status(dir.path(), "worker-1", "running");
        let result = read_agent_status(dir.path(), "worker-1");
        assert_eq!(result.as_deref(), Some("running"));
    }

    #[test]
    fn test_agent_status_waiting_parsed() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_status(dir.path(), "worker-2", "waiting");
        let result = read_agent_status(dir.path(), "worker-2");
        assert_eq!(result.as_deref(), Some("waiting"));
    }

    #[test]
    fn test_agent_status_running_parsed() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_status(dir.path(), "worker-3", "running");
        let result = read_agent_status(dir.path(), "worker-3");
        assert_eq!(result.as_deref(), Some("running"));
    }

    #[test]
    fn test_agent_status_unknown_value_handled() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_status(dir.path(), "worker-4", "some-unexpected-value");
        let result = read_agent_status(dir.path(), "worker-4");
        // Unknown values are returned as-is — callers decide how to handle them
        assert_eq!(result.as_deref(), Some("some-unexpected-value"));
    }

    #[test]
    fn test_agent_status_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_status(dir.path(), "worker-5", "running");
        assert_eq!(
            read_agent_status(dir.path(), "worker-5").as_deref(),
            Some("running")
        );

        write_agent_status(dir.path(), "worker-5", "waiting");
        assert_eq!(
            read_agent_status(dir.path(), "worker-5").as_deref(),
            Some("waiting")
        );
    }

    #[test]
    fn test_agent_status_separate_workers() {
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
