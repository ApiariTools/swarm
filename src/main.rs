mod agent_tui;
mod core;
mod daemon;
mod daemon_tui;
mod tui;

use clap::{Parser, Subcommand};
use color_eyre::Result;

#[derive(Parser)]
#[command(name = "swarm", version, about = "Run agents in parallel.")]
struct Cli {
    /// Working directory (defaults to current dir)
    #[arg(short, long, global = true)]
    dir: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Print swarm state
    Status {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Create a new worktree + agent via the daemon
    Create {
        /// Task prompt (optional if --prompt-file is provided)
        prompt: Option<String>,
        /// Read prompt from a file instead of positional argument
        #[arg(long, value_name = "PATH")]
        prompt_file: Option<String>,
        /// Agent type
        #[arg(long, default_value = "claude-tui")]
        agent: Option<String>,
        /// Repo name (required when multiple repos detected)
        #[arg(long)]
        repo: Option<String>,
        /// Prompt modifiers to prepend (slug: research-first, explore-patterns, or custom .swarm/modifiers/*.md).
        /// Can be specified multiple times: --mod research-first --mod explore-patterns
        #[arg(long = "mod")]
        modifiers: Vec<String>,
    },
    /// Send a message to a worktree's agent
    Send {
        /// Worktree ID
        worktree: String,
        /// Message to send
        message: String,
    },
    /// Close a worktree
    Close {
        /// Worktree ID
        worktree: String,
    },
    /// Merge a worktree
    Merge {
        /// Worktree ID
        worktree: String,
    },
    /// Run the TUI-native Claude agent (standalone)
    AgentTui {
        /// Task prompt
        prompt: Option<String>,
        /// Read prompt from file instead of positional argument
        #[arg(long)]
        prompt_file: Option<String>,
        /// Worktree ID (for event log path)
        #[arg(long)]
        worktree_id: Option<String>,
        /// Skip all permission checks
        #[arg(long)]
        dangerously_skip_permissions: bool,
    },
    /// Attach to a remote daemon via TCP
    Attach {
        /// Remote address (host:port)
        addr: String,
        /// Auth token (will prompt if not provided)
        #[arg(long)]
        token: Option<String>,
    },
    /// Manage the swarm daemon (agent process manager)
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Debug: spawn claude via SDK and print events (no daemon)
    #[command(name = "debug-spawn")]
    DebugSpawn,
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Start the daemon
    Start {
        /// Run in foreground (don't daemonize)
        #[arg(long)]
        foreground: bool,
        /// Enable TCP listener on this address (e.g. 0.0.0.0:9876)
        #[arg(long)]
        bind: Option<String>,
    },
    /// Stop the daemon
    Stop,
    /// Restart the daemon
    Restart {
        /// Run in foreground (don't daemonize)
        #[arg(long)]
        foreground: bool,
        /// Enable TCP listener on this address (e.g. 0.0.0.0:9876)
        #[arg(long)]
        bind: Option<String>,
    },
    /// Show daemon status
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();

    let work_dir = cli
        .dir
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap());

    match cli.command {
        None => run_default_tui(work_dir).await,
        Some(Commands::Status { json }) => cmd_status(work_dir, json),
        Some(Commands::Create {
            prompt,
            prompt_file,
            agent,
            repo,
            modifiers,
        }) => {
            cmd_create(
                work_dir,
                prompt,
                prompt_file,
                agent.unwrap_or_else(|| "claude-tui".to_string()),
                repo,
                modifiers,
            )
            .await
        }
        Some(Commands::Send { worktree, message }) => cmd_send(work_dir, worktree, message).await,
        Some(Commands::Close { worktree }) => cmd_close(work_dir, worktree).await,
        Some(Commands::Merge { worktree }) => cmd_merge(work_dir, worktree).await,
        Some(Commands::AgentTui {
            prompt,
            prompt_file,
            worktree_id,
            dangerously_skip_permissions,
        }) => {
            let prompt = resolve_prompt(prompt, prompt_file).unwrap_or_default();
            agent_tui::run(agent_tui::AgentTuiArgs {
                prompt,
                worktree_id,
                dangerously_skip_permissions,
                work_dir,
            })
            .await
        }
        Some(Commands::Attach { addr, token }) => {
            let token = token.unwrap_or_else(|| {
                eprint!("Auth token: ");
                let mut buf = String::new();
                std::io::stdin().read_line(&mut buf).unwrap_or_default();
                buf.trim().to_string()
            });
            daemon_tui::run_remote(addr, token).await
        }
        Some(Commands::DebugSpawn) => cmd_debug_spawn(work_dir).await,
        Some(Commands::Daemon { action }) => match action {
            DaemonAction::Start { foreground, bind } => {
                daemon::start(work_dir, foreground, bind).await
            }
            DaemonAction::Stop => daemon::stop(&work_dir),
            DaemonAction::Restart { foreground, bind } => {
                daemon::restart(work_dir, foreground, bind).await
            }
            DaemonAction::Status => daemon::status(&work_dir),
        },
    }
}

/// Default command: auto-start daemon if needed, register workspace, then launch the daemon TUI.
async fn run_default_tui(work_dir: std::path::PathBuf) -> Result<()> {
    if !is_daemon_running(&work_dir) {
        eprintln!("[swarm] Starting daemon...");
        let exe = std::env::current_exe()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "swarm".to_string());

        // Log daemon stderr to a file so we can diagnose startup failures.
        let log_dir = work_dir.join(".swarm");
        std::fs::create_dir_all(&log_dir).ok();
        let daemon_log = std::fs::File::create(log_dir.join("daemon-stderr.log"))
            .unwrap_or_else(|_| std::fs::File::open("/dev/null").unwrap());

        std::process::Command::new(&exe)
            .args([
                "-d",
                &work_dir.to_string_lossy(),
                "daemon",
                "start",
                "--foreground",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::from(daemon_log))
            .spawn()
            .map_err(|e| color_eyre::eyre::eyre!("failed to spawn daemon: {}", e))?;

        // Don't wait for daemon readiness — TUI's reconnect loop handles it
    } else {
        // Daemon already running — register workspace in background (don't block TUI startup)
        let bg_dir = work_dir.clone();
        tokio::task::spawn_blocking(move || {
            let _ = core::ipc::send_daemon_request(
                &bg_dir,
                &daemon::protocol::DaemonRequest::RegisterWorkspace {
                    path: bg_dir.clone(),
                },
            );
        });
    }

    daemon_tui::run(work_dir).await
}

/// Check if the swarm daemon is running (global daemon).
fn is_daemon_running(_work_dir: &std::path::Path) -> bool {
    daemon::read_global_pid().is_some_and(daemon::is_process_alive)
}

/// Debug command: spawn claude via SDK (same code path as daemon) and print events.
/// Incrementally adds daemon infrastructure to isolate what breaks child spawning.
async fn cmd_debug_spawn(work_dir: std::path::PathBuf) -> Result<()> {
    eprintln!("[debug-spawn] Step 1: init logging");
    core::log::init(&work_dir);

    eprintln!("[debug-spawn] Step 2: skip global env mutation (child spawn clears CLAUDECODE)");

    eprintln!("[debug-spawn] Step 3: ignore SIGPIPE");
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN); }

    eprintln!("[debug-spawn] Step 4: set up signal handlers");
    let _sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let _sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    eprintln!("[debug-spawn] Step 5: create channels");
    let (event_tx, _) = tokio::sync::broadcast::channel::<daemon::protocol::DaemonResponse>(1024);
    let (_supervisor_tx, _supervisor_rx) = tokio::sync::mpsc::unbounded_channel::<daemon::agent_supervisor::SupervisorEvent>();

    eprintln!("[debug-spawn] Step 6: start socket server");
    let (_request_rx, _socket_handle) =
        daemon::socket_server::start(event_tx.clone(), None, None)?;

    // Use an existing worktree directory if one exists, like the daemon does
    let wt_dir = std::fs::read_dir(work_dir.join(".swarm/wt"))
        .ok()
        .and_then(|mut rd| rd.find_map(|e| e.ok().map(|e| e.path())))
        .unwrap_or(work_dir);
    eprintln!("[debug-spawn] Step 7: spawn claude via SDK inside tokio::spawn (working_dir={})...", wt_dir.display());
    let handle = tokio::spawn(async move {
        let client = apiari_claude_sdk::ClaudeClient::new();
        let opts = apiari_claude_sdk::SessionOptions {
            dangerously_skip_permissions: true,
            include_partial_messages: true,
            working_dir: Some(wt_dir),
            ..Default::default()
        };

        let mut session = match client.spawn(opts).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[debug-spawn] spawn failed: {e}");
                return 0u64;
            }
        };

        eprintln!("[debug-spawn] Spawned. Sending message...");

        if let Err(e) = session
            .send_message("Say hello in exactly 3 words. Nothing else.")
            .await
        {
            eprintln!("[debug-spawn] send failed: {e}");
            return 0;
        }

        eprintln!("[debug-spawn] Message sent. Reading events (stdin kept open)...");

        let mut count = 0u64;
        loop {
            match session.next_event().await {
                Ok(Some(event)) => {
                    count += 1;
                    let is_result = event.is_result();
                    eprintln!("[debug-spawn] event #{count}");
                    if is_result {
                        break;
                    }
                }
                Ok(None) => {
                    eprintln!("[debug-spawn] EOF after {count} events");
                    break;
                }
                Err(e) => {
                    eprintln!("[debug-spawn] ERROR after {count} events: {e}");
                    break;
                }
            }
        }
        count
    });

    let result = tokio::time::timeout(std::time::Duration::from_secs(15), handle).await;
    match result {
        Ok(Ok(count)) => eprintln!("[debug-spawn] Done: {count} events"),
        Ok(Err(e)) => eprintln!("[debug-spawn] PANIC: {e:?}"),
        Err(_) => eprintln!("[debug-spawn] TIMEOUT after 15s"),
    }

    Ok(())
}

// ── IPC Subcommands ────────────────────────────────────────

fn cmd_status(work_dir: std::path::PathBuf, json: bool) -> Result<()> {
    let state = core::state::load_state(&work_dir)?;
    match state {
        Some(s) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&s)?);
            } else {
                println!("worktrees: {}", s.worktrees.len());
                for wt in &s.worktrees {
                    println!(
                        "  {} [{}] {} ({})",
                        wt.id,
                        wt.agent_kind.label(),
                        wt.branch,
                        wt.phase.label(),
                    );
                }
            }
        }
        None => {
            if json {
                println!("null");
            } else {
                println!("no swarm state found");
            }
        }
    }
    Ok(())
}

/// Resolve the task prompt from either the positional argument or --prompt-file.
fn resolve_prompt(prompt: Option<String>, prompt_file: Option<String>) -> Result<String> {
    match (prompt, prompt_file) {
        (_, Some(path)) => {
            let path = std::path::Path::new(&path);
            eprintln!("[swarm] reading prompt from {}", path.display());
            let content = std::fs::read_to_string(path).map_err(|e| {
                color_eyre::eyre::eyre!("failed to read prompt file '{}': {}", path.display(), e)
            })?;
            let content = content.trim().to_string();
            if content.is_empty() {
                return Err(color_eyre::eyre::eyre!(
                    "prompt file '{}' is empty",
                    path.display()
                ));
            }
            eprintln!("[swarm] loaded prompt from file ({} bytes)", content.len());
            Ok(content)
        }
        (Some(prompt), None) => Ok(prompt),
        (None, None) => Err(color_eyre::eyre::eyre!(
            "either a positional <PROMPT> or --prompt-file is required"
        )),
    }
}

async fn cmd_create(
    work_dir: std::path::PathBuf,
    prompt: Option<String>,
    prompt_file: Option<String>,
    agent: String,
    repo: Option<String>,
    modifiers: Vec<String>,
) -> Result<()> {
    let mut prompt = resolve_prompt(prompt, prompt_file)?;

    // Resolve --mod slugs and assemble prompt
    if !modifiers.is_empty() {
        let available = core::modifier::ModifierPrompt::available(&work_dir);
        let slugs: Vec<&str> = available.iter().map(|m| m.slug()).collect();
        let mut resolved = Vec::new();
        for slug in &modifiers {
            let modifier =
                core::modifier::ModifierPrompt::from_slug(slug, &work_dir).ok_or_else(|| {
                    color_eyre::eyre::eyre!(
                        "unknown modifier '{}' (available: {})",
                        slug,
                        slugs.join(", ")
                    )
                })?;
            resolved.push(modifier);
        }
        let selected = vec![true; resolved.len()];
        prompt = core::modifier::assemble_prompt(&prompt, &resolved, &selected);
    }

    // Validate --repo when multiple repos detected
    let repo = if repo.is_some() {
        repo
    } else {
        let repos = core::git::detect_repos(&work_dir)?;
        if repos.len() > 1 {
            let names: Vec<_> = repos.iter().map(|r| core::git::repo_name(r)).collect();
            return Err(color_eyre::eyre::eyre!(
                "multiple repos detected, --repo required: {}",
                names.join(", ")
            ));
        }
        None
    };

    if !is_daemon_running(&work_dir) {
        return Err(color_eyre::eyre::eyre!(
            "daemon not running — start it with `swarm` or `swarm daemon start`"
        ));
    }

    // Register this workspace first (idempotent)
    let _ = core::ipc::send_daemon_request(
        &work_dir,
        &daemon::protocol::DaemonRequest::RegisterWorkspace {
            path: work_dir.clone(),
        },
    );

    let req = daemon::protocol::DaemonRequest::CreateWorker {
        prompt,
        agent,
        repo,
        start_point: None,
        workspace: Some(work_dir.clone()),
    };
    match core::ipc::send_daemon_request(&work_dir, &req) {
        Ok(daemon::protocol::DaemonResponse::Ok { data }) => {
            if let Some(data) = data
                && let Some(wt_id) = data.get("worktree_id").and_then(|v| v.as_str())
            {
                println!("{}", wt_id);
                return Ok(());
            }
            println!("created");
        }
        Ok(daemon::protocol::DaemonResponse::Error { message }) => {
            return Err(color_eyre::eyre::eyre!("{}", message));
        }
        Ok(_) => println!("created"),
        Err(e) => {
            return Err(color_eyre::eyre::eyre!("daemon request failed: {}", e));
        }
    }
    Ok(())
}

async fn cmd_send(work_dir: std::path::PathBuf, worktree: String, message: String) -> Result<()> {
    if !is_daemon_running(&work_dir) {
        return Err(color_eyre::eyre::eyre!(
            "daemon not running — start it with `swarm` or `swarm daemon start`"
        ));
    }
    let req = daemon::protocol::DaemonRequest::SendMessage {
        worktree_id: worktree,
        message,
    };
    match core::ipc::send_daemon_request(&work_dir, &req) {
        Ok(daemon::protocol::DaemonResponse::Ok { .. }) => {
            println!("sent");
        }
        Ok(daemon::protocol::DaemonResponse::Error { message }) => {
            return Err(color_eyre::eyre::eyre!("{}", message));
        }
        Ok(_) => println!("sent"),
        Err(e) => {
            return Err(color_eyre::eyre::eyre!("daemon request failed: {}", e));
        }
    }
    Ok(())
}

async fn cmd_close(work_dir: std::path::PathBuf, worktree: String) -> Result<()> {
    if !is_daemon_running(&work_dir) {
        return Err(color_eyre::eyre::eyre!(
            "daemon not running — start it with `swarm` or `swarm daemon start`"
        ));
    }
    let req = daemon::protocol::DaemonRequest::CloseWorker {
        worktree_id: worktree,
    };
    match core::ipc::send_daemon_request(&work_dir, &req) {
        Ok(daemon::protocol::DaemonResponse::Ok { .. }) => {
            println!("closed");
        }
        Ok(daemon::protocol::DaemonResponse::Error { message }) => {
            return Err(color_eyre::eyre::eyre!("{}", message));
        }
        Ok(_) => println!("closed"),
        Err(e) => {
            return Err(color_eyre::eyre::eyre!("daemon request failed: {}", e));
        }
    }
    Ok(())
}

async fn cmd_merge(work_dir: std::path::PathBuf, worktree: String) -> Result<()> {
    if !is_daemon_running(&work_dir) {
        return Err(color_eyre::eyre::eyre!(
            "daemon not running — start it with `swarm` or `swarm daemon start`"
        ));
    }
    let req = daemon::protocol::DaemonRequest::MergeWorker {
        worktree_id: worktree,
    };
    match core::ipc::send_daemon_request(&work_dir, &req) {
        Ok(daemon::protocol::DaemonResponse::Ok { .. }) => {
            println!("merged");
        }
        Ok(daemon::protocol::DaemonResponse::Error { message }) => {
            return Err(color_eyre::eyre::eyre!("{}", message));
        }
        Ok(_) => println!("merged"),
        Err(e) => {
            return Err(color_eyre::eyre::eyre!("daemon request failed: {}", e));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn resolve_prompt_positional() {
        let result = resolve_prompt(Some("do the thing".into()), None).unwrap();
        assert_eq!(result, "do the thing");
    }

    #[test]
    fn resolve_prompt_from_file() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(tmp, "build the feature\nwith multiple lines").unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        let result = resolve_prompt(None, Some(path)).unwrap();
        assert_eq!(result, "build the feature\nwith multiple lines");
    }

    #[test]
    fn resolve_prompt_file_overrides_positional() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(tmp, "from file").unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        let result = resolve_prompt(Some("from arg".into()), Some(path)).unwrap();
        assert_eq!(result, "from file");
    }

    #[test]
    fn resolve_prompt_empty_file_errors() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        let err = resolve_prompt(None, Some(path)).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn resolve_prompt_missing_file_errors() {
        let err = resolve_prompt(None, Some("/no/such/file.txt".into())).unwrap_err();
        assert!(err.to_string().contains("failed to read"));
    }

    #[test]
    fn resolve_prompt_neither_errors() {
        let err = resolve_prompt(None, None).unwrap_err();
        assert!(err.to_string().contains("either"));
    }

    #[test]
    fn resolve_prompt_trims_whitespace() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(tmp, "  trimmed prompt  \n").unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        let result = resolve_prompt(None, Some(path)).unwrap();
        assert_eq!(result, "trimmed prompt");
    }
}
