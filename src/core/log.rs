use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

static LOG_FILE: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

/// Open `.swarm/swarm.log` in append mode for the duration of the process.
pub fn init(work_dir: &Path) {
    let path = work_dir.join(".swarm").join("swarm.log");
    if let Ok(file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = LOG_FILE.set(Mutex::new(file));
    }
}

/// Write a timestamped line to the log file. No-op if `init` was not called.
pub fn log(msg: &str) {
    if let Some(mtx) = LOG_FILE.get()
        && let Ok(mut f) = mtx.lock()
    {
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
        let _ = writeln!(f, "[{now}] {msg}");
    }
}

/// `format!`-style logging to `.swarm/swarm.log`.
#[macro_export]
macro_rules! swarm_log {
    ($($arg:tt)*) => {
        $crate::core::log::log(&format!($($arg)*))
    };
}
