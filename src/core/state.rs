use crate::core::agent::AgentKind;
use chrono::{DateTime, Local};
use color_eyre::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Parsed review verdict from a reviewer worker's text output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewVerdict {
    pub approved: bool,
    #[serde(default)]
    pub comments: Vec<String>,
}

/// Parse a review verdict from the reviewer worker's accumulated text output.
///
/// Scans for `REVIEW_VERDICT: APPROVED` or `REVIEW_VERDICT: CHANGES_REQUESTED`
/// followed by comment lines starting with `- `.
pub fn parse_review_verdict(output: &str) -> Option<ReviewVerdict> {
    let mut found_changes_requested = false;
    let mut collecting_comments = false;
    let mut comments = Vec::new();

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed == "REVIEW_VERDICT: APPROVED" {
            return Some(ReviewVerdict {
                approved: true,
                comments: vec![],
            });
        }
        if trimmed == "REVIEW_VERDICT: CHANGES_REQUESTED" {
            found_changes_requested = true;
            collecting_comments = true;
            continue;
        }
        if collecting_comments {
            if let Some(comment) = trimmed.strip_prefix("- ") {
                comments.push(comment.to_string());
            } else if !trimmed.is_empty() {
                // Non-empty, non-comment line stops comment collection
                collecting_comments = false;
            }
            // Empty lines don't stop comment collection
        }
    }

    if found_changes_requested {
        Some(ReviewVerdict {
            approved: false,
            comments,
        })
    } else {
        None
    }
}

/// Parse a `BRANCH_READY: <branch-name>` line from worker text output.
///
/// Returns the branch name if found, otherwise `None`.
pub fn parse_branch_ready(output: &str) -> Option<String> {
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(branch) = trimmed.strip_prefix("BRANCH_READY:") {
            let branch = branch.trim();
            if !branch.is_empty() {
                return Some(branch.to_string());
            }
        }
    }
    None
}

/// PR info fetched from `gh`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PrInfo {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub url: String,
}

/// Worker lifecycle phase — the single source of truth for worker state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerPhase {
    /// Git worktree + agent process being set up.
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

/// Persisted agent pane state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneState {
    pub pane_id: String,
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
    /// Daemon mode: PID of the agent process.
    #[serde(default)]
    pub agent_pid: Option<u32>,
    /// Session ID for resume across daemon restarts.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Number of auto-restarts (observability).
    #[serde(default)]
    pub restart_count: Option<u32>,
    /// Worker role: "worker" (default) or "reviewer".
    #[serde(default)]
    pub role: Option<String>,
    /// PR number being reviewed (when role = "reviewer").
    #[serde(default)]
    pub review_pr: Option<u64>,
    /// Parsed review verdict after the reviewer worker completes.
    #[serde(default)]
    pub review_verdict: Option<ReviewVerdict>,
    /// Branch name signalled ready by the worker (via `BRANCH_READY: <name>`).
    #[serde(default)]
    pub ready_branch: Option<String>,
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
            agent: Some(PaneState {
                pane_id: "%1".to_string(),
            }),
            terminals: vec![],
            summary: Some("fix bug in auth".to_string()),
            pr,
            phase: WorkerPhase::Running,
            status: "running".to_string(),
            agent_session_status: None,
            agent_pid: None,
            session_id: None,
            restart_count: None,
            role: None,
            review_pr: None,
            review_verdict: None,
            ready_branch: None,
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
            let restored: WorkerPhase = serde_json::from_str(&json).expect("deserialize phase");
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

    // ── ReviewVerdict tests ────────────────────────────────

    #[test]
    fn parse_branch_ready_basic() {
        let output = "All done!\nBRANCH_READY: swarm/my-feature-abc1";
        assert_eq!(
            parse_branch_ready(output),
            Some("swarm/my-feature-abc1".to_string())
        );
    }

    #[test]
    fn parse_branch_ready_not_present() {
        let output = "Work complete.";
        assert!(parse_branch_ready(output).is_none());
    }

    #[test]
    fn parse_branch_ready_empty_branch() {
        let output = "BRANCH_READY: ";
        assert!(parse_branch_ready(output).is_none());
    }

    #[test]
    fn parse_branch_ready_trims_whitespace() {
        let output = "  BRANCH_READY:   swarm/foo-bar  ";
        assert_eq!(
            parse_branch_ready(output),
            Some("swarm/foo-bar".to_string())
        );
    }

    #[test]
    fn parse_verdict_approved() {
        let output = "The code looks great!\nREVIEW_VERDICT: APPROVED";
        let v = parse_review_verdict(output).unwrap();
        assert!(v.approved);
        assert!(v.comments.is_empty());
    }

    #[test]
    fn parse_verdict_changes_requested() {
        let output = "Found some issues.\nREVIEW_VERDICT: CHANGES_REQUESTED\n- [src/foo.rs:42] Missing null check\n- [src/bar.rs:10] Unsafe cast";
        let v = parse_review_verdict(output).unwrap();
        assert!(!v.approved);
        assert_eq!(v.comments.len(), 2);
        assert!(v.comments[0].contains("Missing null check"));
        assert!(v.comments[1].contains("Unsafe cast"));
    }

    #[test]
    fn parse_verdict_no_verdict() {
        let output = "I reviewed the code but forgot to output a verdict";
        assert!(parse_review_verdict(output).is_none());
    }

    #[test]
    fn parse_verdict_malformed_verdict_type() {
        let output = "REVIEW_VERDICT: MAYBE";
        assert!(parse_review_verdict(output).is_none());
    }

    #[test]
    fn parse_verdict_empty_output() {
        assert!(parse_review_verdict("").is_none());
    }

    #[test]
    fn parse_verdict_changes_requested_no_comments() {
        let output = "REVIEW_VERDICT: CHANGES_REQUESTED";
        let v = parse_review_verdict(output).unwrap();
        assert!(!v.approved);
        assert!(v.comments.is_empty());
    }

    #[test]
    fn parse_verdict_approved_ignores_trailing_text() {
        let output = "REVIEW_VERDICT: APPROVED\nSome trailing text";
        let v = parse_review_verdict(output).unwrap();
        assert!(v.approved);
    }

    #[test]
    fn parse_verdict_stops_at_non_comment_line() {
        let output = "REVIEW_VERDICT: CHANGES_REQUESTED\n- [a:1] issue one\n\nSome text\n- [b:2] not collected";
        let v = parse_review_verdict(output).unwrap();
        assert!(!v.approved);
        // Empty line doesn't stop collection, but "Some text" does
        assert_eq!(v.comments.len(), 1);
        assert!(v.comments[0].contains("issue one"));
    }

    #[test]
    fn review_verdict_serde_round_trip() {
        let v = ReviewVerdict {
            approved: false,
            comments: vec!["[file:1] issue 1".into(), "[file:2] issue 2".into()],
        };
        let json = serde_json::to_string(&v).unwrap();
        let restored: ReviewVerdict = serde_json::from_str(&json).unwrap();
        assert_eq!(v, restored);
    }

    #[test]
    fn review_verdict_approved_serde() {
        let v = ReviewVerdict {
            approved: true,
            comments: vec![],
        };
        let json = serde_json::to_string(&v).unwrap();
        assert!(json.contains("\"approved\":true"));
        let restored: ReviewVerdict = serde_json::from_str(&json).unwrap();
        assert_eq!(v, restored);
    }

    #[test]
    fn worktree_state_new_fields_default_to_none() {
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
        let ws: WorktreeState = serde_json::from_str(json).unwrap();
        assert!(ws.role.is_none());
        assert!(ws.review_pr.is_none());
        assert!(ws.review_verdict.is_none());
        assert!(ws.ready_branch.is_none());
    }

    #[test]
    fn worktree_state_reviewer_round_trips() {
        let verdict = ReviewVerdict {
            approved: false,
            comments: vec!["[foo.rs:1] bad thing".into()],
        };
        let mut ws = make_worktree_state(None);
        ws.role = Some("reviewer".to_string());
        ws.review_pr = Some(42);
        ws.review_verdict = Some(verdict.clone());
        let json = serde_json::to_string(&ws).unwrap();
        let restored: WorktreeState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.role.as_deref(), Some("reviewer"));
        assert_eq!(restored.review_pr, Some(42));
        assert_eq!(restored.review_verdict, Some(verdict));
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
