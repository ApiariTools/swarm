// Only compiled when used as a library (server feature); the binary doesn't need this.
#[cfg(feature = "server")]
pub mod a2a_state;
pub mod agent;
pub mod agent_card;
pub mod config;
pub mod git;
pub mod ipc;
pub mod log;
pub mod merge;
pub mod modifier;
pub mod prerequisites;
pub mod profile;
pub mod shell;
pub mod state;
