use a2a_types::TaskState;

use super::state::WorkerPhase;

/// Map a [`WorkerPhase`] and optional agent session status to an A2A [`TaskState`].
///
/// Mapping:
/// - `Creating` | `Starting` | `Running` → `Working`
/// - `Waiting` (or agent_session_status == "waiting") → `InputRequired`
/// - `Completed` → `Completed`
/// - `Failed` → `Failed`
pub fn worktree_to_task_state(
    phase: &WorkerPhase,
    agent_session_status: Option<&str>,
) -> TaskState {
    // Agent session status can override: if the agent reports "waiting", treat as InputRequired
    // even if the phase hasn't caught up yet.
    if agent_session_status == Some("waiting") {
        return TaskState::InputRequired;
    }

    match phase {
        WorkerPhase::Creating | WorkerPhase::Starting | WorkerPhase::Running => TaskState::Working,
        WorkerPhase::Waiting => TaskState::InputRequired,
        WorkerPhase::Completed => TaskState::Completed,
        WorkerPhase::Failed => TaskState::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creating_maps_to_working() {
        assert_eq!(
            worktree_to_task_state(&WorkerPhase::Creating, None),
            TaskState::Working
        );
    }

    #[test]
    fn starting_maps_to_working() {
        assert_eq!(
            worktree_to_task_state(&WorkerPhase::Starting, None),
            TaskState::Working
        );
    }

    #[test]
    fn running_maps_to_working() {
        assert_eq!(
            worktree_to_task_state(&WorkerPhase::Running, None),
            TaskState::Working
        );
    }

    #[test]
    fn waiting_maps_to_input_required() {
        assert_eq!(
            worktree_to_task_state(&WorkerPhase::Waiting, None),
            TaskState::InputRequired
        );
    }

    #[test]
    fn completed_maps_to_completed() {
        assert_eq!(
            worktree_to_task_state(&WorkerPhase::Completed, None),
            TaskState::Completed
        );
    }

    #[test]
    fn failed_maps_to_failed() {
        assert_eq!(
            worktree_to_task_state(&WorkerPhase::Failed, None),
            TaskState::Failed
        );
    }

    #[test]
    fn agent_session_waiting_overrides_running_phase() {
        assert_eq!(
            worktree_to_task_state(&WorkerPhase::Running, Some("waiting")),
            TaskState::InputRequired
        );
    }

    #[test]
    fn agent_session_running_does_not_override() {
        assert_eq!(
            worktree_to_task_state(&WorkerPhase::Running, Some("running")),
            TaskState::Working
        );
    }

    #[test]
    fn agent_session_none_does_not_override() {
        assert_eq!(
            worktree_to_task_state(&WorkerPhase::Completed, None),
            TaskState::Completed
        );
    }
}
