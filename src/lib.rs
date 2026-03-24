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
/// The `agent` and `state` submodules are always available. Additional
/// submodules (e.g. `ipc`) are gated behind the `client` feature.
/// Binary-only modules (git, shell, etc.) live exclusively in the
/// `swarm` binary crate.
pub mod core {
    pub mod agent;
    // Exposed under `client` for socket_path / global_socket_path only.
    // The module is public so downstream can reach it, but the re-exports
    // at the crate root are the intended API surface.
    #[cfg(all(unix, feature = "client"))]
    pub(crate) mod ipc;
    pub mod state;
}

/// Daemon protocol types and IPC client (Unix-only, requires `client` feature).
#[cfg(all(unix, feature = "client"))]
pub mod daemon {
    pub mod ipc_client;
    pub mod protocol;
}

// Re-export the most commonly used types at the crate root for convenience.
pub use core::agent::AgentKind;
pub use core::state::{
    PaneState, PrInfo, SwarmState, WorkerPhase, WorktreeState, load_state, save_state, state_path,
};

// Client re-exports — kept under a `client` sub-module to avoid polluting
// the crate root namespace and to prevent naming conflicts.
#[cfg(all(unix, feature = "client"))]
pub mod client {
    //! Convenience re-exports for daemon IPC consumers (Unix-only).
    pub use crate::core::ipc::{global_socket_path, socket_path};
    pub use crate::daemon::ipc_client::send_daemon_request;
    pub use crate::daemon::protocol::{
        AgentEventWire, DaemonRequest, DaemonResponse, TaskDirPayload, WorkerInfo, WorkspaceInfo,
    };
}
