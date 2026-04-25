//! A2A HTTP/SSE server for the swarm daemon.
//!
//! Exposes standard A2A endpoints so any A2A-compliant client can discover
//! workers and stream task updates without using the Unix socket protocol.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use serde::Deserialize;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;

use super::protocol::{DaemonRequest, DaemonResponse, WorkerInfo};
use crate::core::agent_card::build_agent_card;
use crate::core::profile::DEFAULT_PROFILE;

// ── Shared state ─────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    /// Send requests into the daemon event loop.
    request_tx: mpsc::UnboundedSender<(DaemonRequest, mpsc::UnboundedSender<DaemonResponse>)>,
    /// Subscribe to daemon broadcast events (A2aTaskUpdate, StateChanged, …).
    event_tx: broadcast::Sender<DaemonResponse>,
}

// ── Server handle ─────────────────────────────────────────

/// Handle to the running A2A HTTP server.
/// Aborts the server task on drop.
pub struct A2aHttpHandle {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for A2aHttpHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// ── Start ────────────────────────────────────────────────

/// Start the A2A HTTP server.
///
/// `port = 0` asks the OS to assign a free port.
/// Returns the server handle and the actual bound port.
pub async fn start(
    port: u16,
    request_tx: mpsc::UnboundedSender<(DaemonRequest, mpsc::UnboundedSender<DaemonResponse>)>,
    event_tx: broadcast::Sender<DaemonResponse>,
) -> color_eyre::Result<(A2aHttpHandle, u16)> {
    let state = Arc::new(AppState {
        request_tx,
        event_tx,
    });

    let app = Router::new()
        .route("/.well-known/agent.json", get(agent_card_handler))
        .route("/a2a/workers", get(list_workers_handler))
        .route("/a2a/workers/{worker_id}", get(get_worker_handler))
        .route(
            "/a2a/workers/{worker_id}/tasks/send",
            post(send_message_handler),
        )
        .route(
            "/a2a/workers/{worker_id}/tasks/events",
            get(worker_events_handler),
        )
        .route("/a2a/events", get(all_events_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    let bound_port = listener.local_addr()?.port();

    tracing::info!("A2A HTTP server listening on http://0.0.0.0:{bound_port}");

    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("A2A HTTP server error: {e}");
        }
    });

    Ok((A2aHttpHandle { task }, bound_port))
}

// ── Daemon request helper ─────────────────────────────────

/// Send a request to the daemon event loop and await the first response.
async fn send_daemon_request(
    state: &AppState,
    request: DaemonRequest,
) -> Result<DaemonResponse, String> {
    let (resp_tx, mut resp_rx) = mpsc::unbounded_channel();
    state
        .request_tx
        .send((request, resp_tx))
        .map_err(|e| format!("daemon channel closed: {e}"))?;
    resp_rx
        .recv()
        .await
        .ok_or_else(|| "daemon returned no response".to_string())
}

/// Convert a daemon error string into an HTTP 500 JSON response.
fn daemon_error(msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": msg.into() })),
    )
}

// ── GET /.well-known/agent.json ──────────────────────────

async fn agent_card_handler() -> Json<serde_json::Value> {
    let card = build_agent_card("swarm-daemon", "swarm", "swarm", DEFAULT_PROFILE);
    Json(serde_json::to_value(&card).unwrap_or_default())
}

// ── GET /a2a/workers ─────────────────────────────────────

async fn list_workers_handler(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<WorkerInfo>>, (StatusCode, Json<serde_json::Value>)> {
    match send_daemon_request(&state, DaemonRequest::ListWorkers { workspace: None }).await {
        Ok(DaemonResponse::Workers { workers }) => Ok(Json(workers)),
        Ok(DaemonResponse::Error { message }) => Err(daemon_error(message)),
        Ok(_) => Err(daemon_error("unexpected daemon response")),
        Err(e) => Err(daemon_error(e)),
    }
}

// ── GET /a2a/workers/{worker_id} ──────────────────────────

#[derive(serde::Serialize)]
struct WorkerDetail {
    #[serde(flatten)]
    info: WorkerInfo,
    task_state: a2a_types::TaskState,
}

async fn get_worker_handler(
    Path(worker_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<WorkerDetail>, (StatusCode, Json<serde_json::Value>)> {
    match send_daemon_request(&state, DaemonRequest::ListWorkers { workspace: None }).await {
        Ok(DaemonResponse::Workers { workers }) => {
            let worker = workers.into_iter().find(|w| w.id == worker_id);
            match worker {
                Some(info) => {
                    let task_state = phase_to_task_state(&info.phase);
                    Ok(Json(WorkerDetail { info, task_state }))
                }
                None => Err((
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": format!("unknown worker: {worker_id}") })),
                )),
            }
        }
        Ok(DaemonResponse::Error { message }) => Err(daemon_error(message)),
        Ok(_) => Err(daemon_error("unexpected daemon response")),
        Err(e) => Err(daemon_error(e)),
    }
}

fn phase_to_task_state(phase: &crate::core::state::WorkerPhase) -> a2a_types::TaskState {
    use crate::core::state::WorkerPhase;
    match phase {
        WorkerPhase::Creating | WorkerPhase::Starting => a2a_types::TaskState::Submitted,
        WorkerPhase::Running => a2a_types::TaskState::Working,
        WorkerPhase::Waiting => a2a_types::TaskState::InputRequired,
        WorkerPhase::Completed => a2a_types::TaskState::Completed,
        WorkerPhase::Failed => a2a_types::TaskState::Failed,
    }
}

// ── POST /a2a/workers/{worker_id}/tasks/send ──────────────

/// Body for the send endpoint.
/// Accepts `{"message": {"role": "user", "parts": [{"type": "text", "text": "..."}]}}`.
#[derive(Deserialize)]
struct SendBody {
    message: MessagePayload,
}

#[derive(Deserialize)]
struct MessagePayload {
    #[allow(dead_code)]
    role: Option<String>,
    parts: Vec<PartPayload>,
}

#[derive(Deserialize)]
struct PartPayload {
    #[serde(rename = "type")]
    #[allow(dead_code)]
    part_type: Option<String>,
    text: Option<String>,
}

async fn send_message_handler(
    Path(worker_id): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<SendBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // Flatten all text parts into a single message string.
    let message: String = body
        .message
        .parts
        .into_iter()
        .filter_map(|p| p.text)
        .collect::<Vec<_>>()
        .join("\n");

    if message.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "message text is empty" })),
        ));
    }

    match send_daemon_request(
        &state,
        DaemonRequest::SendMessage {
            worktree_id: worker_id,
            message,
        },
    )
    .await
    {
        Ok(DaemonResponse::Ok { .. }) => Ok(Json(serde_json::json!({ "status": "ok" }))),
        Ok(DaemonResponse::Error { message }) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": message })),
        )),
        Ok(_) => Err(daemon_error("unexpected daemon response")),
        Err(e) => Err(daemon_error(e)),
    }
}

// ── GET /a2a/workers/{worker_id}/tasks/events (SSE) ───────

async fn worker_events_handler(
    Path(worker_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.event_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |msg| {
        let worker_id = worker_id.clone();
        match msg {
            Ok(DaemonResponse::A2aTaskUpdate {
                worktree_id,
                task_state,
                message,
                timestamp,
            }) if worktree_id == worker_id => {
                let data = serde_json::json!({
                    "task_state": task_state,
                    "message": message,
                    "timestamp": timestamp,
                });
                Some(Ok(Event::default()
                    .event("task_update")
                    .data(serde_json::to_string(&data).unwrap_or_default())))
            }
            _ => None,
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ── GET /a2a/events (SSE, all workers) ───────────────────

async fn all_events_handler(
    State(state): State<Arc<AppState>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.event_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|msg| match msg {
        Ok(DaemonResponse::A2aTaskUpdate {
            worktree_id,
            task_state,
            message,
            timestamp,
        }) => {
            let data = serde_json::json!({
                "worktree_id": worktree_id,
                "task_state": task_state,
                "message": message,
                "timestamp": timestamp,
            });
            Some(Ok(Event::default()
                .event("task_update")
                .data(serde_json::to_string(&data).unwrap_or_default())))
        }
        _ => None,
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}
