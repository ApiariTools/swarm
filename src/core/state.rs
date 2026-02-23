use crate::core::agent::AgentKind;
use crate::tui::app::PrInfo;
use chrono::{DateTime, Local};
use color_eyre::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Persisted tmux pane state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneState {
    pub pane_id: String,
}

// Backward compat: deserialize old "tmux_target" field as "pane_id"
impl PaneState {
    pub fn new(pane_id: String) -> Self {
        Self { pane_id }
    }
}

/// Persisted worktree state (survives restarts).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeState {
    pub id: String,
    pub branch: String,
    pub prompt: String,
    pub agent_kind: AgentKind,
    pub repo_path: PathBuf,
    pub worktree_path: PathBuf,
    pub created_at: DateTime<Local>,
    pub agent: Option<PaneState>,
    #[serde(default)]
    pub terminals: Vec<PaneState>,
    #[serde(default)]
    pub summary: Option<String>,
    /// PR info (number, title, state, URL) if a PR exists for this worktree's branch.
    #[serde(default)]
    pub pr: Option<PrInfo>,
    /// Agent status: "running" or "done". Computed at serialization time,
    /// not persisted (defaults to "running" when loading from disk).
    #[serde(default = "default_status")]
    pub status: String,
    /// Claude-tui session status (e.g. "waiting", "running"). Read from
    /// `.swarm/agent-status/<worktree_id>` so hive can detect when a
    /// worker is waiting for input.
    #[serde(default, skip_deserializing)]
    pub agent_session_status: Option<String>,
}

fn default_status() -> String {
    "running".to_string()
}

/// All swarm state for a workspace.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SwarmState {
    pub session_name: String,
    #[serde(default)]
    pub sidebar_pane_id: Option<String>,
    pub worktrees: Vec<WorktreeState>,
    /// Byte offset into inbox.jsonl — messages before this offset have already been processed.
    #[serde(default)]
    pub last_inbox_pos: u64,
}

/// Get the state file path.
pub fn state_path(work_dir: &Path) -> PathBuf {
    work_dir.join(".swarm").join("state.json")
}

/// Load state from disk.
///
/// Returns `None` if the state file does not exist.
pub fn load_state(work_dir: &Path) -> Result<Option<SwarmState>> {
    let path = state_path(work_dir);
    if !path.exists() {
        return Ok(None);
    }
    let state: SwarmState = apiari_common::state::load_state(&path)?;
    Ok(Some(state))
}

/// Save state to disk (atomic write via temp file + rename).
pub fn save_state(work_dir: &Path, state: &SwarmState) -> Result<()> {
    let path = state_path(work_dir);
    apiari_common::state::save_state(&path, state)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_worktree_state(pr: Option<PrInfo>) -> WorktreeState {
        WorktreeState {
            id: "test-1".to_string(),
            branch: "swarm/test-1".to_string(),
            prompt: "fix the bug".to_string(),
            agent_kind: AgentKind::Claude,
            repo_path: PathBuf::from("/tmp/repo"),
            worktree_path: PathBuf::from("/tmp/repo/.swarm/wt/test-1"),
            created_at: Local::now(),
            agent: Some(PaneState::new("%1".to_string())),
            terminals: vec![],
            summary: Some("fix bug in auth".to_string()),
            pr,
            status: "running".to_string(),
            agent_session_status: None,
        }
    }

    #[test]
    fn worktree_with_pr_round_trips() {
        let pr = PrInfo {
            number: 42,
            title: "Fix auth bug".to_string(),
            state: "OPEN".to_string(),
            url: "https://github.com/ApiariTools/swarm/pull/42".to_string(),
        };
        let ws = make_worktree_state(Some(pr));
        let json = serde_json::to_string(&ws).expect("serialize");
        let restored: WorktreeState = serde_json::from_str(&json).expect("deserialize");

        let pr = restored.pr.expect("pr should be Some");
        assert_eq!(pr.number, 42);
        assert_eq!(pr.title, "Fix auth bug");
        assert_eq!(pr.state, "OPEN");
        assert_eq!(pr.url, "https://github.com/ApiariTools/swarm/pull/42");
    }

    #[test]
    fn worktree_without_pr_round_trips() {
        let ws = make_worktree_state(None);
        let json = serde_json::to_string(&ws).expect("serialize");
        let restored: WorktreeState = serde_json::from_str(&json).expect("deserialize");

        assert!(restored.pr.is_none());
    }

    #[test]
    fn old_state_without_pr_field_deserializes() {
        // Simulate state.json from before the pr field existed
        let json = r#"{
            "id": "test-1",
            "branch": "swarm/test-1",
            "prompt": "fix the bug",
            "agent_kind": "claude",
            "repo_path": "/tmp/repo",
            "worktree_path": "/tmp/repo/.swarm/wt/test-1",
            "created_at": "2025-01-01T00:00:00-05:00",
            "agent": {"pane_id": "%1"},
            "terminals": [],
            "summary": null,
            "status": "running"
        }"#;
        let restored: WorktreeState = serde_json::from_str(json).expect("deserialize old format");
        assert!(restored.pr.is_none());
    }
}
