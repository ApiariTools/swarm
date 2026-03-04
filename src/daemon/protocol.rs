use crate::core::ipc::InboxMessage;
use crate::core::state::WorkerPhase;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// Request sent to the daemon over the Unix socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DaemonRequest {
    ListWorkers {
        /// Filter by workspace (None = all workspaces).
        #[serde(default)]
        workspace: Option<PathBuf>,
    },
    CreateWorker {
        prompt: String,
        #[serde(default = "default_agent")]
        agent: String,
        #[serde(default)]
        repo: Option<String>,
        #[serde(default)]
        start_point: Option<String>,
        /// Which workspace to create the worker in.
        #[serde(default)]
        workspace: Option<PathBuf>,
    },
    SendMessage {
        worktree_id: String,
        message: String,
    },
    CloseWorker {
        worktree_id: String,
    },
    MergeWorker {
        worktree_id: String,
    },
    Subscribe {
        /// None = subscribe to all workers.
        #[serde(default)]
        worktree_id: Option<String>,
        /// Filter events by workspace (None = all).
        #[serde(default)]
        workspace: Option<PathBuf>,
    },
    GetHistory {
        worktree_id: String,
    },
    /// Authenticate a TCP connection with a token.
    Auth {
        token: String,
    },
    /// Register a workspace with the daemon.
    RegisterWorkspace {
        path: PathBuf,
    },
    /// Unregister a workspace from the daemon.
    UnregisterWorkspace {
        path: PathBuf,
    },
    /// List all registered workspaces.
    ListWorkspaces,
    /// Trigger an immediate PR poll for specific workers.
    TriggerPrPoll {
        worker_ids: Vec<String>,
    },
    Ping,
}

fn default_agent() -> String {
    "claude".to_string()
}

/// Response sent by the daemon back to clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DaemonResponse {
    Ok {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
    Error {
        message: String,
    },
    Workers {
        workers: Vec<WorkerInfo>,
    },
    Workspaces {
        workspaces: Vec<WorkspaceInfo>,
    },
    AgentEvent {
        worktree_id: String,
        event: AgentEventWire,
    },
    StateChanged {
        worktree_id: String,
        phase: WorkerPhase,
    },
}

/// Summary info about a registered workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub path: PathBuf,
    pub worker_count: usize,
}

/// Summary info about a worker, returned by ListWorkers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerInfo {
    pub id: String,
    pub branch: String,
    pub prompt: String,
    pub agent: String,
    pub phase: WorkerPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_state: Option<String>,
    #[serde(default)]
    pub restart_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Local>>,
}

/// Wire-format agent events streamed to subscribers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEventWire {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    ToolUse {
        tool: String,
        input: String,
    },
    ToolResult {
        output: String,
        is_error: bool,
    },
    TurnComplete,
    SessionResult {
        #[serde(default)]
        turns: u64,
        #[serde(default)]
        cost_usd: Option<f64>,
        #[serde(default)]
        session_id: Option<String>,
    },
    SessionWaiting {
        session_id: String,
    },
    Error {
        message: String,
    },
}

/// Try to translate a legacy InboxMessage into a DaemonRequest.
/// This allows old-style socket clients (hive daemon, CLI) to keep working.
pub fn translate_inbox_message(msg: &InboxMessage) -> DaemonRequest {
    match msg {
        InboxMessage::Create {
            prompt,
            agent,
            repo,
            start_point,
            ..
        } => DaemonRequest::CreateWorker {
            prompt: prompt.clone(),
            agent: agent.clone(),
            repo: repo.clone(),
            start_point: start_point.clone(),
            workspace: None,
        },
        InboxMessage::Send {
            worktree, message, ..
        } => DaemonRequest::SendMessage {
            worktree_id: worktree.clone(),
            message: message.clone(),
        },
        InboxMessage::Close { worktree, .. } => DaemonRequest::CloseWorker {
            worktree_id: worktree.clone(),
        },
        InboxMessage::Merge { worktree, .. } => DaemonRequest::MergeWorker {
            worktree_id: worktree.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_request_create_round_trips() {
        let req = DaemonRequest::CreateWorker {
            prompt: "fix the bug".into(),
            agent: "claude".into(),
            repo: Some("hive".into()),
            start_point: None,
            workspace: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"action\":\"create_worker\""));
        let restored: DaemonRequest = serde_json::from_str(&json).unwrap();
        match restored {
            DaemonRequest::CreateWorker { prompt, agent, .. } => {
                assert_eq!(prompt, "fix the bug");
                assert_eq!(agent, "claude");
            }
            _ => panic!("expected CreateWorker"),
        }
    }

    #[test]
    fn daemon_request_list_workers() {
        let json = r#"{"action":"list_workers"}"#;
        let req: DaemonRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(req, DaemonRequest::ListWorkers { workspace: None }));
    }

    #[test]
    fn daemon_request_list_workers_with_workspace() {
        let json = r#"{"action":"list_workers","workspace":"/tmp/foo"}"#;
        let req: DaemonRequest = serde_json::from_str(json).unwrap();
        match req {
            DaemonRequest::ListWorkers { workspace } => {
                assert_eq!(workspace, Some(PathBuf::from("/tmp/foo")));
            }
            _ => panic!("expected ListWorkers"),
        }
    }

    #[test]
    fn daemon_request_register_workspace() {
        let req = DaemonRequest::RegisterWorkspace {
            path: PathBuf::from("/tmp/my-project"),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"action\":\"register_workspace\""));
        let restored: DaemonRequest = serde_json::from_str(&json).unwrap();
        match restored {
            DaemonRequest::RegisterWorkspace { path } => {
                assert_eq!(path, PathBuf::from("/tmp/my-project"));
            }
            _ => panic!("expected RegisterWorkspace"),
        }
    }

    #[test]
    fn daemon_request_list_workspaces() {
        let json = r#"{"action":"list_workspaces"}"#;
        let req: DaemonRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(req, DaemonRequest::ListWorkspaces));
    }

    #[test]
    fn daemon_response_workspaces_round_trips() {
        let resp = DaemonResponse::Workspaces {
            workspaces: vec![WorkspaceInfo {
                path: PathBuf::from("/tmp/project"),
                worker_count: 3,
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"kind\":\"workspaces\""));
        let restored: DaemonResponse = serde_json::from_str(&json).unwrap();
        match restored {
            DaemonResponse::Workspaces { workspaces } => {
                assert_eq!(workspaces.len(), 1);
                assert_eq!(workspaces[0].path, PathBuf::from("/tmp/project"));
                assert_eq!(workspaces[0].worker_count, 3);
            }
            _ => panic!("expected Workspaces"),
        }
    }

    #[test]
    fn daemon_request_subscribe_all() {
        let json = r#"{"action":"subscribe"}"#;
        let req: DaemonRequest = serde_json::from_str(json).unwrap();
        match req {
            DaemonRequest::Subscribe { worktree_id, .. } => assert!(worktree_id.is_none()),
            _ => panic!("expected Subscribe"),
        }
    }

    #[test]
    fn daemon_request_subscribe_specific() {
        let json = r#"{"action":"subscribe","worktree_id":"hive-1"}"#;
        let req: DaemonRequest = serde_json::from_str(json).unwrap();
        match req {
            DaemonRequest::Subscribe { worktree_id, .. } => {
                assert_eq!(worktree_id.as_deref(), Some("hive-1"));
            }
            _ => panic!("expected Subscribe"),
        }
    }

    #[test]
    fn daemon_response_ok_round_trips() {
        let resp = DaemonResponse::Ok { data: None };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"kind\":\"ok\""));
        let restored: DaemonResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(restored, DaemonResponse::Ok { data: None }));
    }

    #[test]
    fn daemon_response_error_round_trips() {
        let resp = DaemonResponse::Error {
            message: "not found".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let restored: DaemonResponse = serde_json::from_str(&json).unwrap();
        match restored {
            DaemonResponse::Error { message } => assert_eq!(message, "not found"),
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn daemon_response_workers_round_trips() {
        let resp = DaemonResponse::Workers {
            workers: vec![WorkerInfo {
                id: "hive-1".into(),
                branch: "swarm/fix-bug-1".into(),
                prompt: "fix the bug".into(),
                agent: "claude".into(),
                phase: WorkerPhase::Running,
                session_id: Some("sess-123".into()),
                pr_url: None,
                pr_number: None,
                pr_title: None,
                pr_state: None,
                restart_count: 0,
                created_at: None,
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        let restored: DaemonResponse = serde_json::from_str(&json).unwrap();
        match restored {
            DaemonResponse::Workers { workers } => {
                assert_eq!(workers.len(), 1);
                assert_eq!(workers[0].id, "hive-1");
            }
            _ => panic!("expected Workers"),
        }
    }

    #[test]
    fn agent_event_wire_text_delta() {
        let event = AgentEventWire::TextDelta {
            text: "hello".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"text_delta\""));
        let restored: AgentEventWire = serde_json::from_str(&json).unwrap();
        match restored {
            AgentEventWire::TextDelta { text } => assert_eq!(text, "hello"),
            _ => panic!("expected TextDelta"),
        }
    }

    #[test]
    fn agent_event_wire_session_result() {
        let event = AgentEventWire::SessionResult {
            turns: 5,
            cost_usd: Some(0.10),
            session_id: Some("sess-abc".into()),
        };
        let json = serde_json::to_string(&event).unwrap();
        let restored: AgentEventWire = serde_json::from_str(&json).unwrap();
        match restored {
            AgentEventWire::SessionResult {
                turns,
                cost_usd,
                session_id,
            } => {
                assert_eq!(turns, 5);
                assert_eq!(cost_usd, Some(0.10));
                assert_eq!(session_id.as_deref(), Some("sess-abc"));
            }
            _ => panic!("expected SessionResult"),
        }
    }

    #[test]
    fn agent_event_wire_tool_use() {
        let event = AgentEventWire::ToolUse {
            tool: "Bash".into(),
            input: "ls -la".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let restored: AgentEventWire = serde_json::from_str(&json).unwrap();
        match restored {
            AgentEventWire::ToolUse { tool, input } => {
                assert_eq!(tool, "Bash");
                assert_eq!(input, "ls -la");
            }
            _ => panic!("expected ToolUse"),
        }
    }

    #[test]
    fn translate_inbox_create_to_daemon_request() {
        let msg = InboxMessage::Create {
            id: "msg-1".into(),
            prompt: "fix auth".into(),
            agent: "claude".into(),
            repo: Some("hive".into()),
            start_point: None,
            timestamp: chrono::Local::now(),
        };
        let req = translate_inbox_message(&msg);
        match req {
            DaemonRequest::CreateWorker { prompt, agent, .. } => {
                assert_eq!(prompt, "fix auth");
                assert_eq!(agent, "claude");
            }
            _ => panic!("expected CreateWorker"),
        }
    }

    #[test]
    fn translate_inbox_send_to_daemon_request() {
        let msg = InboxMessage::Send {
            id: "msg-2".into(),
            worktree: "hive-1".into(),
            message: "please review".into(),
            timestamp: chrono::Local::now(),
        };
        let req = translate_inbox_message(&msg);
        match req {
            DaemonRequest::SendMessage {
                worktree_id,
                message,
            } => {
                assert_eq!(worktree_id, "hive-1");
                assert_eq!(message, "please review");
            }
            _ => panic!("expected SendMessage"),
        }
    }

    #[test]
    fn daemon_request_ping() {
        let json = r#"{"action":"ping"}"#;
        let req: DaemonRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(req, DaemonRequest::Ping));
    }

    #[test]
    fn daemon_request_auth_round_trips() {
        let req = DaemonRequest::Auth {
            token: "my-secret-token".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"action\":\"auth\""));
        let restored: DaemonRequest = serde_json::from_str(&json).unwrap();
        match restored {
            DaemonRequest::Auth { token } => assert_eq!(token, "my-secret-token"),
            _ => panic!("expected Auth"),
        }
    }

    #[test]
    fn daemon_request_trigger_pr_poll_round_trips() {
        let req = DaemonRequest::TriggerPrPoll {
            worker_ids: vec!["hive-1".into(), "hive-2".into()],
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"action\":\"trigger_pr_poll\""));
        let restored: DaemonRequest = serde_json::from_str(&json).unwrap();
        match restored {
            DaemonRequest::TriggerPrPoll { worker_ids } => {
                assert_eq!(worker_ids, vec!["hive-1", "hive-2"]);
            }
            _ => panic!("expected TriggerPrPoll"),
        }
    }

    #[test]
    fn daemon_request_defaults_agent_to_claude() {
        let json = r#"{"action":"create_worker","prompt":"test"}"#;
        let req: DaemonRequest = serde_json::from_str(json).unwrap();
        match req {
            DaemonRequest::CreateWorker { agent, workspace, .. } => {
                assert_eq!(agent, "claude");
                assert!(workspace.is_none()); // defaults to None
            }
            _ => panic!("expected CreateWorker"),
        }
    }
}
