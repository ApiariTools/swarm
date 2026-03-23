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

/// Core swarm types: agent kinds, worktree/swarm state, and state I/O.
///
/// Only the library-safe submodules (`agent`, `state`) are exposed here.
/// Binary-only modules (IPC, git, daemon, etc.) live exclusively in the
/// `swarm` binary crate.
pub mod core {
    pub mod agent;
    pub mod state;
}

// Re-export the most commonly used types at the crate root for convenience.
pub use core::agent::AgentKind;
pub use core::state::{
    PaneState, PrInfo, SwarmState, WorkerPhase, WorktreeState, load_state, save_state, state_path,
};
