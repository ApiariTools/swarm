//! Integration test verifying that `client` feature exposes the expected types
//! and they serialize/deserialize correctly.

#![cfg(all(unix, feature = "client"))]

use apiari_swarm::client::{
    AgentEventWire, DaemonRequest, DaemonResponse, WorkerInfo, global_socket_path,
    send_daemon_request, socket_path,
};

#[test]
fn daemon_request_ping_round_trips() {
    let req = DaemonRequest::Ping;
    let json = serde_json::to_string(&req).unwrap();
    let restored: DaemonRequest = serde_json::from_str(&json).unwrap();
    assert!(matches!(restored, DaemonRequest::Ping));
}

#[test]
fn daemon_response_ok_round_trips() {
    let resp = DaemonResponse::Ok { data: None };
    let json = serde_json::to_string(&resp).unwrap();
    let restored: DaemonResponse = serde_json::from_str(&json).unwrap();
    assert!(matches!(restored, DaemonResponse::Ok { data: None }));
}

#[test]
fn agent_event_wire_round_trips() {
    let event = AgentEventWire::TextDelta {
        text: "hello".into(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let restored: AgentEventWire = serde_json::from_str(&json).unwrap();
    match restored {
        AgentEventWire::TextDelta { text } => assert_eq!(text, "hello"),
        _ => panic!("expected TextDelta"),
    }
}

#[test]
fn socket_path_helpers_are_accessible() {
    let path = socket_path(std::path::Path::new("/tmp/test"));
    assert!(path.to_string_lossy().contains("swarm.sock"));

    let global = global_socket_path();
    assert!(global.to_string_lossy().contains("swarm.sock"));
}

#[test]
fn send_daemon_request_uses_nonexistent_path() {
    // Use a unique temp dir that cannot have a real daemon socket,
    // avoiding flakiness when a live daemon is running on the machine.
    let dir = tempfile::tempdir().unwrap();
    let result = send_daemon_request(dir.path(), &DaemonRequest::Ping);
    assert!(result.is_err());
}

#[test]
fn worker_info_is_accessible() {
    // Verify WorkerInfo is usable from external code
    let info = WorkerInfo {
        id: "test".into(),
        branch: "swarm/test".into(),
        prompt: "fix it".into(),
        agent: "claude".into(),
        phase: apiari_swarm::WorkerPhase::Running,
        session_id: None,
        pr_url: None,
        pr_number: None,
        pr_title: None,
        pr_state: None,
        restart_count: 0,
        created_at: None,
        agent_card: None,
    };
    let json = serde_json::to_string(&info).unwrap();
    assert!(json.contains("\"id\":\"test\""));
}
