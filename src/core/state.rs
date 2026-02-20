use crate::core::agent::AgentKind;
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
}

/// All swarm state for a workspace.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SwarmState {
    pub session_name: String,
    #[serde(default)]
    pub sidebar_pane_id: Option<String>,
    pub worktrees: Vec<WorktreeState>,
}

/// Get the state file path.
pub fn state_path(work_dir: &Path) -> PathBuf {
    work_dir.join(".swarm").join("state.json")
}

/// Load state from disk.
pub fn load_state(work_dir: &Path) -> Result<Option<SwarmState>> {
    let path = state_path(work_dir);
    if !path.exists() {
        return Ok(None);
    }

    let data = std::fs::read_to_string(&path)?;
    let state: SwarmState = serde_json::from_str(&data)?;
    Ok(Some(state))
}

/// Save state to disk.
pub fn save_state(work_dir: &Path, state: &SwarmState) -> Result<()> {
    let path = state_path(work_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let data = serde_json::to_string_pretty(state)?;
    std::fs::write(&path, data)?;
    Ok(())
}
