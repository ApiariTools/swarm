use std::process::Command;

/// Check that required CLI tools are available before spawning an agent.
///
/// Returns `Err` if `claude` is missing (hard requirement).
/// Logs a warning if `gh` is missing (optional, needed for PR tracking).
pub fn check_prerequisites() -> Result<(), String> {
    // Hard requirement: claude CLI
    if !is_command_available("claude") {
        return Err("claude CLI not found. Install it from https://claude.ai/code".to_string());
    }

    // Soft requirement: gh CLI (warn only)
    if !is_command_available("gh") {
        tracing::warn!("gh CLI not found — PR tracking will be disabled");
    }

    Ok(())
}

/// Check whether a command is available in PATH by running `<cmd> --version`.
fn is_command_available(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_command_available_finds_existing_command() {
        // `echo` should always be available
        assert!(is_command_available("echo"));
    }

    #[test]
    fn is_command_available_returns_false_for_missing() {
        assert!(!is_command_available("this-command-does-not-exist-12345"));
    }
}
