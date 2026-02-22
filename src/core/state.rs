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
    #[serde(default)]
    pub summary: Option<String>,
    /// Agent status: "running" or "done". Computed at serialization time,
    /// not persisted (defaults to "running" when loading from disk).
    #[serde(default = "default_status")]
    pub status: String,
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
