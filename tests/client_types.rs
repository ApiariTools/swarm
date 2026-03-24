//! Integration test verifying that `client` feature exposes the expected types
//! and they serialize/deserialize correctly.

#![cfg(feature = "client")]

use apiari_swarm::{
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
    // Just verify the functions are callable from external code
    let path = socket_path(std::path::Path::new("/tmp/test"));
    assert!(path.to_string_lossy().contains("swarm.sock"));

    let global = global_socket_path();
    assert!(global.to_string_lossy().contains("swarm.sock"));
}

#[test]
fn send_daemon_request_is_accessible() {
    // Verify the function signature is accessible (will fail to connect, but that's expected)
    let result = send_daemon_request(
        std::path::Path::new("/tmp/nonexistent-swarm-test"),
        &DaemonRequest::Ping,
    );
    assert!(result.is_err()); // No daemon running, but the function is callable
}
