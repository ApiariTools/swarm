use std::path::PathBuf;
use std::process::Output;

/// Trait for running external commands (git, gh, tmux).
///
/// Production code uses [`RealCommandRunner`]. Tests can inject a mock
/// (via `mockall::automock`) to avoid spawning real subprocesses.
///
/// NOTE: The daemon and git modules currently call `std::process::Command`
/// directly. A follow-up PR should thread `CommandRunner` through
/// `core::git` and `daemon::mod` so that `handle_request` tests can
/// verify git/gh invocations without touching the filesystem.
#[cfg_attr(test, mockall::automock)]
pub trait CommandRunner: Send + Sync {
    fn run(&self, cmd: String, args: Vec<String>, cwd: PathBuf) -> std::io::Result<Output>;
}

/// Default implementation that delegates to `std::process::Command`.
pub struct RealCommandRunner;

impl CommandRunner for RealCommandRunner {
    fn run(&self, cmd: String, args: Vec<String>, cwd: PathBuf) -> std::io::Result<Output> {
        std::process::Command::new(&cmd)
            .args(&args)
            .current_dir(&cwd)
            .output()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockall::predicate::*;

    #[test]
    fn mock_runner_returns_configured_output() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run()
            .with(
                eq("echo".to_string()),
                eq(vec!["hello".to_string()]),
                always(),
            )
            .returning(|_, _, _| {
                Ok(Output {
                    status: std::process::ExitStatus::default(),
                    stdout: b"hello\n".to_vec(),
                    stderr: vec![],
                })
            });

        let output = mock
            .run(
                "echo".to_string(),
                vec!["hello".to_string()],
                PathBuf::from("/tmp"),
            )
            .unwrap();
        assert_eq!(output.stdout, b"hello\n");
    }

    #[test]
    fn mock_runner_can_simulate_failure() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run().returning(|_, _, _| {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "command not found",
            ))
        });

        let result = mock.run(
            "nonexistent".to_string(),
            vec![],
            PathBuf::from("/tmp"),
        );
        assert!(result.is_err());
    }
}
