//! Swarm library — expose core types for external consumers.
//!
//! This library re-exports the essential swarm state management types that
//! external crates (like `apiari`) need to read and interact with swarm state.
//!
//! # Core Modules
//!
//! - [`core::state`] — Worktree and swarm state types (`WorktreeState`, `SwarmState`)
//! - [`core::agent`] — Agent kind enumeration (`AgentKind`)
//!
//! # Features
//!
//! The TUI modules (`tui`, `daemon_tui`, `agent_tui`) and their heavy dependencies
//! (`ratatui`, `crossterm`) are gated behind the `tui` feature (enabled by default).
//! For lightweight lib-only builds, use `default-features = false`.
//!
//! The `client` feature (Unix-only) exposes the daemon IPC protocol types and
//! a helper to talk to the swarm daemon over its Unix socket. It exposes:
//! - [`daemon::protocol`] — request/response enums and wire types
//! - [`daemon::ipc_client`] — `send_daemon_request` helper
//! - Socket path helpers from [`core::ipc`]

/// Core swarm types: agent kinds, worktree/swarm state, and state I/O.
///
/// When the `server` feature is enabled, the full set of core submodules is
/// available (git, config, log, merge, etc.). Otherwise only the subset needed
/// by library consumers (agent, state, profile, etc.) is exposed.
#[cfg(all(unix, feature = "server"))]
#[path = "core/mod.rs"]
pub mod core;

#[cfg(not(all(unix, feature = "server")))]
pub mod core {
    pub mod a2a_state;
    pub mod agent;
    pub mod agent_card;
    #[cfg(all(unix, feature = "client"))]
    pub(crate) mod ipc;
    pub mod profile;
    pub mod state;
}

/// Full daemon module (Unix-only, requires `server` feature).
///
/// Exposes `daemon::start()`, `daemon::stop()`, `daemon::lifecycle`, and all
/// internal submodules needed to run the swarm daemon as a library.
#[cfg(all(unix, feature = "server"))]
#[path = "daemon/mod.rs"]
pub mod daemon;

/// Daemon protocol types and IPC client only (Unix-only, requires `client` feature, no `server`).
#[cfg(all(unix, feature = "client", not(feature = "server")))]
pub mod daemon {
    pub mod ipc_client;
    pub mod protocol;
}

// Re-export the most commonly used types at the crate root for convenience.
pub use core::a2a_state::worktree_to_task_state;
pub use core::agent::AgentKind;
pub use core::agent_card::build_agent_card;
pub use core::state::{
    PaneState, PrInfo, SwarmState, WorkerPhase, WorktreeState, load_state, save_state, state_path,
};

// Client re-exports — kept under a `client` sub-module to avoid polluting
// the crate root namespace and to prevent naming conflicts.
#[cfg(all(unix, any(feature = "client", feature = "server")))]
pub mod client {
    //! Convenience re-exports for daemon IPC consumers (Unix-only).
    pub use crate::core::ipc::{global_socket_path, socket_path};
    pub use crate::daemon::ipc_client::send_daemon_request;
    pub use crate::daemon::protocol::{
        AgentEventWire, DaemonRequest, DaemonResponse, TaskDirPayload, WorkerInfo, WorkspaceInfo,
    };
}
