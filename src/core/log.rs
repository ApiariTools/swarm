//! Logging initialization — replaces the old manual OnceLock file logger.

use std::path::Path;

/// Initialize stderr logging for CLI subcommands.
/// Respects RUST_LOG env var; defaults to INFO with timestamps.
pub fn init_stderr() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(true)
        .init();
}

/// Initialize file logging for daemon mode. Writes to `{work_dir}/.swarm/swarm.log`.
/// CRITICAL: the returned WorkerGuard must be stored for the entire process lifetime.
/// Dropping it shuts down the background flush thread and silently loses log lines.
pub fn init(work_dir: &Path) -> tracing_appender::non_blocking::WorkerGuard {
    use tracing_subscriber::{EnvFilter, fmt};
    let log_dir = work_dir.join(".swarm");
    std::fs::create_dir_all(&log_dir).ok();
    let appender = tracing_appender::rolling::never(&log_dir, "swarm.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // Use try_init so this is a no-op when a subscriber is already set
    // (e.g. when run_daemon is called in-process after init_stderr).
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(false)
        .try_init();
    guard
}
