use std::path::Path;
use std::process::Output;

/// Abstraction over running external commands (git, gh, tmux).
///
/// Production code uses RealCommandRunner; tests can inject a mock
/// (via mockall) to verify command invocations without spawning processes.
///
/// **Note**: This trait is defined but not yet wired into App. Doing so
/// requires threading the runner through App, create_worktree_with_agent,
/// and all the tmux/git call sites -- a refactor deferred to a follow-up PR.
#[allow(dead_code)]
#[cfg_attr(test, mockall::automock)]
pub trait CommandRunner: Send + Sync {
    fn run(&self, cmd: &str, args: &[String], cwd: &Path) -> std::io::Result<Output>;
}

/// Runs commands via std::process::Command (the real implementation).
#[allow(dead_code)]
pub struct RealCommandRunner;

impl CommandRunner for RealCommandRunner {
    fn run(&self, cmd: &str, args: &[String], cwd: &Path) -> std::io::Result<Output> {
        std::process::Command::new(cmd)
            .args(args)
            .current_dir(cwd)
            .output()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn real_runner_executes_echo() {
        let runner = RealCommandRunner;
        let args = vec!["hello".to_string()];
        let output = runner
            .run("echo", &args, Path::new("/tmp"))
            .expect("echo should succeed");
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
    }

    #[test]
    fn mock_runner_returns_configured_output() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run()
            .withf(|cmd, args, _cwd| cmd == "git" && args == ["status"])
            .returning(|_, _, _| {
                Ok(Output {
                    status: std::process::ExitStatus::from_raw(0),
                    stdout: b"on branch main".to_vec(),
                    stderr: Vec::new(),
                })
            });

        let args = vec!["status".to_string()];
        let output = mock.run("git", &args, Path::new("/tmp")).unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("main"));
    }
}
