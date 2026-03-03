//! Trait for running external commands (git, gh, tmux) so tests can inject a mock.

use std::path::Path;
use std::process::Output;

#[cfg(test)]
use mockall::automock;

/// Abstraction over running shell commands. Production code uses
/// [`RealCommandRunner`]; tests can use `MockCommandRunner` (generated
/// by mockall) to avoid touching the filesystem or spawning processes.
#[cfg_attr(test, automock)]
pub trait CommandRunner: Send + Sync {
    fn run(&self, cmd: &str, args: &[String], cwd: &Path) -> std::io::Result<Output>;
}

/// The real implementation — delegates to `std::process::Command`.
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
    fn real_runner_can_execute_echo() {
        let runner = RealCommandRunner;
        let args: Vec<String> = vec!["hello".to_string()];
        let output = runner
            .run("echo", &args, Path::new("/tmp"))
            .expect("echo should succeed");
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "hello"
        );
    }

    #[test]
    fn mock_runner_returns_canned_output() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run()
            .withf(|cmd, args, _cwd| {
                cmd == "git" && args == vec!["status".to_string()]
            })
            .returning(|_, _, _| {
                Ok(Output {
                    status: std::process::ExitStatus::from_raw(0),
                    stdout: b"clean".to_vec(),
                    stderr: vec![],
                })
            });

        let args = vec!["status".to_string()];
        let out = mock.run("git", &args, Path::new("/tmp")).unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "clean");
    }
}
