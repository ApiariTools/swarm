//! Integration test for the `client` feature.
//!
//! Verifies that external crates can access daemon protocol types
//! and serialize/deserialize them correctly.

#[cfg(feature = "client")]
mod client_feature_tests {
    use apiari_swarm::{
        AgentEventWire, DaemonRequest, DaemonResponse, global_socket_path, socket_path,
    };
    use std::path::PathBuf;

    #[test]
    fn test_daemon_request_ping_serializes() {
        let req = DaemonRequest::Ping;
        let json = serde_json::to_string(&req).expect("failed to serialize");
        assert!(json.contains("\"action\":\"ping\""));
    }

    #[test]
    fn test_daemon_request_list_workers_round_trip() {
        let req = DaemonRequest::ListWorkers {
            workspace: Some(PathBuf::from("/tmp/workspace")),
        };
        let json = serde_json::to_string(&req).expect("failed to serialize");
        let restored: DaemonRequest = serde_json::from_str(&json).expect("failed to deserialize");
        match restored {
            DaemonRequest::ListWorkers { workspace } => {
                assert_eq!(workspace, Some(PathBuf::from("/tmp/workspace")));
            }
            _ => panic!("expected ListWorkers variant"),
        }
    }

    #[test]
    fn test_daemon_response_ok_round_trip() {
        let resp = DaemonResponse::Ok { data: None };
        let json = serde_json::to_string(&resp).expect("failed to serialize");
        let restored: DaemonResponse = serde_json::from_str(&json).expect("failed to deserialize");
        assert!(matches!(restored, DaemonResponse::Ok { data: None }));
    }

    #[test]
    fn test_daemon_response_error_round_trip() {
        let resp = DaemonResponse::Error {
            message: "test error".to_string(),
        };
        let json = serde_json::to_string(&resp).expect("failed to serialize");
        let restored: DaemonResponse = serde_json::from_str(&json).expect("failed to deserialize");
        match restored {
            DaemonResponse::Error { message } => {
                assert_eq!(message, "test error");
            }
            _ => panic!("expected Error variant"),
        }
    }

    #[test]
    fn test_agent_event_wire_text_delta() {
        let event = AgentEventWire::TextDelta {
            text: "hello world".to_string(),
        };
        let json = serde_json::to_string(&event).expect("failed to serialize");
        assert!(json.contains("\"type\":\"text_delta\""));
        let restored: AgentEventWire = serde_json::from_str(&json).expect("failed to deserialize");
        match restored {
            AgentEventWire::TextDelta { text } => {
                assert_eq!(text, "hello world");
            }
            _ => panic!("expected TextDelta variant"),
        }
    }

    #[test]
    fn test_socket_path_functions_accessible() {
        // Test that socket path functions are accessible from the crate root
        let work_dir = PathBuf::from("/tmp/test-workspace");
        let local_sock = socket_path(&work_dir);
        assert!(local_sock.ends_with(".swarm/swarm.sock"));

        let global_sock = global_socket_path();
        assert!(global_sock.ends_with(".config/swarm/swarm.sock"));
    }

    #[test]
    fn test_daemon_request_create_worker_minimal() {
        let req = DaemonRequest::CreateWorker {
            prompt: "fix the bug".to_string(),
            agent: "claude".to_string(),
            repo: None,
            start_point: None,
            workspace: None,
            profile: None,
            task_dir: None,
        };
        let json = serde_json::to_string(&req).expect("failed to serialize");
        assert!(json.contains("\"action\":\"create_worker\""));
        assert!(json.contains("\"prompt\":\"fix the bug\""));
    }

    #[test]
    fn test_daemon_request_send_message() {
        let req = DaemonRequest::SendMessage {
            worktree_id: "hive-abc123".to_string(),
            message: "please review the PR".to_string(),
        };
        let json = serde_json::to_string(&req).expect("failed to serialize");
        let restored: DaemonRequest = serde_json::from_str(&json).expect("failed to deserialize");
        match restored {
            DaemonRequest::SendMessage {
                worktree_id,
                message,
            } => {
                assert_eq!(worktree_id, "hive-abc123");
                assert_eq!(message, "please review the PR");
            }
            _ => panic!("expected SendMessage variant"),
        }
    }
}
