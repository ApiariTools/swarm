//! Read/write agent-status files under `.swarm/agent-status/<worktree_id>`.
//!
//! These single-line files let external tools (hive, keeper) detect whether
//! an agent is "running", "waiting", etc.

#![allow(dead_code)]

use std::path::Path;

/// Directory within the workspace for agent-status files.
fn status_dir(work_dir: &Path) -> std::path::PathBuf {
    work_dir.join(".swarm").join("agent-status")
}

/// Write the agent status file.
pub fn write_agent_status(work_dir: &Path, worktree_id: &str, status: &str) {
    let dir = status_dir(work_dir);
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join(worktree_id), status);
}

/// Read the agent status file. Returns `None` if the file doesn't exist or
/// can't be read.
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
    fn test_agent_status_waiting_parsed() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_status(dir.path(), "worker-1", "waiting");
        let result = read_agent_status(dir.path(), "worker-1");
        assert_eq!(result.as_deref(), Some("waiting"));
    }

    #[test]
    fn test_agent_status_running_parsed() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_status(dir.path(), "worker-2", "running");
        let result = read_agent_status(dir.path(), "worker-2");
        assert_eq!(result.as_deref(), Some("running"));
    }

    #[test]
    fn test_agent_status_unknown_value_handled() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_status(dir.path(), "worker-3", "some-unexpected-value");
        // Should still return the value, not panic or error
        let result = read_agent_status(dir.path(), "worker-3");
        assert_eq!(result.as_deref(), Some("some-unexpected-value"));
    }

    #[test]
    fn test_agent_status_trims_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        // Manually write with trailing newline
        let status_dir = dir.path().join(".swarm").join("agent-status");
        std::fs::create_dir_all(&status_dir).unwrap();
        std::fs::write(status_dir.join("worker-4"), "running\n").unwrap();
        let result = read_agent_status(dir.path(), "worker-4");
        assert_eq!(result.as_deref(), Some("running"));
    }

    #[test]
    fn test_agent_status_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_status(dir.path(), "worker-5", "running");
        assert_eq!(read_agent_status(dir.path(), "worker-5").as_deref(), Some("running"));

        write_agent_status(dir.path(), "worker-5", "waiting");
        assert_eq!(read_agent_status(dir.path(), "worker-5").as_deref(), Some("waiting"));
    }

    #[test]
    fn test_agent_status_separate_workers() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_status(dir.path(), "worker-a", "running");
        write_agent_status(dir.path(), "worker-b", "waiting");

        assert_eq!(read_agent_status(dir.path(), "worker-a").as_deref(), Some("running"));
        assert_eq!(read_agent_status(dir.path(), "worker-b").as_deref(), Some("waiting"));
    }
}
