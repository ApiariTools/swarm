//! Swarm library — expose core types for external consumers.
//!
//! This library re-exports the essential swarm state management types that
//! external crates (like `apiari`) need to read and interact with swarm state.
//!
//! # Core Modules
//!
//! - [`state`] — Worktree and swarm state types (`WorktreeState`, `SwarmState`)
//! - [`agent`] — Agent kind enumeration (`AgentKind`)
//!
//! # Features
//!
//! The TUI modules (`tui`, `daemon_tui`, `agent_tui`), daemon, IPC, and their dependencies are not exposed
//! by default, as they depend on heavy dependencies like `ratatui` and `crossterm`.
//! If you need full swarm functionality (daemon, IPC, TUI), use the binary (`apiari-swarm`) directly.

pub mod core;

// Re-export the most commonly used types at the crate root for convenience
pub use core::agent::AgentKind;
pub use core::state::{
    load_state, save_state, state_path, PaneState, PrInfo, SwarmState, WorkerPhase,
    WorktreeState,
};
