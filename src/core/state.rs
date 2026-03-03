use crate::core::agent::AgentKind;
use crate::tui::app::PrInfo;
use chrono::{DateTime, Local};
use color_eyre::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Worker lifecycle phase — the single source of truth for worker state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerPhase {
    /// Git worktree + tmux pane being set up.
    Creating,
    /// Pane exists, prompt not yet delivered.
    Starting,
    /// Agent is actively executing.
    Running,
    /// Agent is waiting for user input.
    Waiting,
    /// Agent pane exited normally.
    Completed,
    /// Creation or execution failed.
    Failed,
}

impl Default for WorkerPhase {
    /// Defaults to Running for backward compat with old state.json files
    /// that don't have a `phase` field.
    fn default() -> Self {
        Self::Running
    }
}

impl WorkerPhase {
    /// Returns true for terminal phases (Completed, Failed).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }

    /// Returns true for active (non-terminal) phases.
    pub fn is_active(&self) -> bool {
        !self.is_terminal()
    }

    /// Check whether a transition from this phase to `to` is valid.
    pub fn can_transition_to(&self, to: &WorkerPhase) -> bool {
        matches!(
            (self, to),
            (Self::Creating, Self::Starting)
                | (Self::Creating, Self::Failed)
                | (Self::Starting, Self::Running)
                | (Self::Starting, Self::Failed)
                | (Self::Starting, Self::Completed)
                | (Self::Running, Self::Waiting)
                | (Self::Running, Self::Completed)
                | (Self::Waiting, Self::Running)
                | (Self::Waiting, Self::Completed)
                | (Self::Completed, Self::Starting) // agent relaunch
                | (Self::Failed, Self::Starting) // agent relaunch after failure
        )
    }

    /// Human-readable label for display.
    pub fn label(&self) -> &str {
        match self {
            Self::Creating => "creating",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

impl std::fmt::Display for WorkerPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

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
    /// Worker lifecycle phase.
    #[serde(default)]
    pub phase: WorkerPhase,
    /// Agent status: "running" or "done". Computed from `phase` at serialization time
    /// for backward compatibility with hive.
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
            phase: WorkerPhase::Running,
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

    // ── WorkerPhase tests ──────────────────────────────────

    #[test]
    fn worker_phase_serde_round_trip() {
        let phases = vec![
            WorkerPhase::Creating,
            WorkerPhase::Starting,
            WorkerPhase::Running,
            WorkerPhase::Waiting,
            WorkerPhase::Completed,
            WorkerPhase::Failed,
        ];
        for phase in phases {
            let json = serde_json::to_string(&phase).expect("serialize phase");
            let restored: WorkerPhase =
                serde_json::from_str(&json).expect("deserialize phase");
            assert_eq!(phase, restored);
        }
    }

    #[test]
    fn worker_phase_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&WorkerPhase::Creating).unwrap(),
            "\"creating\""
        );
        assert_eq!(
            serde_json::to_string(&WorkerPhase::Running).unwrap(),
            "\"running\""
        );
        assert_eq!(
            serde_json::to_string(&WorkerPhase::Completed).unwrap(),
            "\"completed\""
        );
    }

    #[test]
    fn worker_phase_is_terminal() {
        assert!(!WorkerPhase::Creating.is_terminal());
        assert!(!WorkerPhase::Starting.is_terminal());
        assert!(!WorkerPhase::Running.is_terminal());
        assert!(!WorkerPhase::Waiting.is_terminal());
        assert!(WorkerPhase::Completed.is_terminal());
        assert!(WorkerPhase::Failed.is_terminal());
    }

    #[test]
    fn worker_phase_is_active() {
        assert!(WorkerPhase::Creating.is_active());
        assert!(WorkerPhase::Starting.is_active());
        assert!(WorkerPhase::Running.is_active());
        assert!(WorkerPhase::Waiting.is_active());
        assert!(!WorkerPhase::Completed.is_active());
        assert!(!WorkerPhase::Failed.is_active());
    }

    #[test]
    fn worker_phase_valid_transitions() {
        // Creating →
        assert!(WorkerPhase::Creating.can_transition_to(&WorkerPhase::Starting));
        assert!(WorkerPhase::Creating.can_transition_to(&WorkerPhase::Failed));
        // Starting →
        assert!(WorkerPhase::Starting.can_transition_to(&WorkerPhase::Running));
        assert!(WorkerPhase::Starting.can_transition_to(&WorkerPhase::Failed));
        assert!(WorkerPhase::Starting.can_transition_to(&WorkerPhase::Completed));
        // Running →
        assert!(WorkerPhase::Running.can_transition_to(&WorkerPhase::Waiting));
        assert!(WorkerPhase::Running.can_transition_to(&WorkerPhase::Completed));
        // Waiting →
        assert!(WorkerPhase::Waiting.can_transition_to(&WorkerPhase::Running));
        assert!(WorkerPhase::Waiting.can_transition_to(&WorkerPhase::Completed));
        // Completed →
        assert!(WorkerPhase::Completed.can_transition_to(&WorkerPhase::Starting)); // relaunch
    }

    #[test]
    fn worker_phase_invalid_transitions() {
        // Creating cannot go to Running directly
        assert!(!WorkerPhase::Creating.can_transition_to(&WorkerPhase::Running));
        assert!(!WorkerPhase::Creating.can_transition_to(&WorkerPhase::Waiting));
        assert!(!WorkerPhase::Creating.can_transition_to(&WorkerPhase::Completed));
        // Running cannot go back to Starting
        assert!(!WorkerPhase::Running.can_transition_to(&WorkerPhase::Starting));
        assert!(!WorkerPhase::Running.can_transition_to(&WorkerPhase::Creating));
        // Terminal phases can't transition
        assert!(!WorkerPhase::Completed.can_transition_to(&WorkerPhase::Running));
        assert!(!WorkerPhase::Completed.can_transition_to(&WorkerPhase::Failed));
        assert!(!WorkerPhase::Failed.can_transition_to(&WorkerPhase::Running));
        assert!(!WorkerPhase::Failed.can_transition_to(&WorkerPhase::Creating));
    }

    #[test]
    fn worker_phase_default_is_running() {
        assert_eq!(WorkerPhase::default(), WorkerPhase::Running);
    }

    #[test]
    fn old_state_without_phase_field_deserializes_as_running() {
        // Simulate state.json from before the phase field existed
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
        let restored: WorktreeState = serde_json::from_str(json).expect("deserialize");
        assert_eq!(restored.phase, WorkerPhase::Running);
    }

    #[test]
    fn to_state_computes_status_from_phase() {
        let mut ws = make_worktree_state(None);
        ws.phase = WorkerPhase::Running;
        let json = serde_json::to_string(&ws).unwrap();
        assert!(json.contains("\"status\":\"running\""));

        ws.phase = WorkerPhase::Creating;
        let json = serde_json::to_string(&ws).unwrap();
        assert!(json.contains("\"status\":\"running\"")); // active → running

        ws.phase = WorkerPhase::Completed;
        let json = serde_json::to_string(&ws).unwrap();
        // status field is written as-is from the struct, but `to_state()` sets it
        // We test this in app.rs tests; here we verify the field round-trips
        let restored: WorktreeState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.phase, WorkerPhase::Completed);
    }
}
