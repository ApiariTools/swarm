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
        /// Profile slug from .swarm/profiles/ (default: "default").
        #[arg(long, default_value = "default")]
        profile: String,
        /// Path to JSON file with .task/ artifacts to seed.
        #[arg(long, value_name = "PATH")]
        task_dir: Option<String>,
        /// Worker role: "worker" (default) writes code and opens PRs; "reviewer" reviews a PR diff.
        #[arg(long, default_value = "worker")]
        role: String,
        /// PR number to review (required when --role reviewer).
        #[arg(long)]
        pr: Option<u64>,
        /// Base branch for diff (used with --role reviewer, default: "main").
        #[arg(long, default_value = "main")]
        base_branch: String,
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
    /// Adopt an existing GitHub PR into a swarm worker
    Adopt {
        /// GitHub PR number to adopt
        #[arg(long)]
        pr: u64,
        /// Task prompt
        #[arg(long)]
        prompt: Option<String>,
        /// Read prompt from file
        #[arg(long, value_name = "PATH")]
        prompt_file: Option<String>,
        /// Agent type
        #[arg(long, default_value = "claude-tui")]
        agent: Option<String>,
        /// Repo name
        #[arg(long)]
        repo: Option<String>,
        /// Profile slug
        #[arg(long, default_value = "default")]
        profile: String,
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

    // Strip CLAUDECODE-injected GH_TOKEN — it's often a sandbox token that
    // overrides the user's real `gh auth login` credentials and causes 401s.
    if std::env::var("CLAUDECODE").is_ok() {
        unsafe {
            std::env::remove_var("GH_TOKEN");
        }
        unsafe {
            std::env::remove_var("GITHUB_TOKEN");
        }
    }

    let cli = Cli::parse();

    // Daemon subcommands initialize their own file logger inside run_daemon().
    // All other subcommands get stderr logging initialized here.
    match &cli.command {
        Some(Commands::Daemon { .. }) => {}
        _ => core::log::init_stderr(),
    }

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
            profile,
            task_dir,
            role,
            pr,
            base_branch,
        }) => {
            cmd_create(
                work_dir,
                prompt,
                prompt_file,
                agent.unwrap_or_else(|| "claude-tui".to_string()),
                repo,
                modifiers,
                profile,
                task_dir,
                role,
                pr,
                base_branch,
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
        Some(Commands::Adopt {
            pr,
            prompt,
            prompt_file,
            agent,
            repo,
            profile,
        }) => {
            cmd_adopt(
                work_dir,
                pr,
                prompt,
                prompt_file,
                agent.unwrap_or_else(|| "claude-tui".to_string()),
                repo,
                profile,
            )
            .await
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
    // Show onboarding screen on first launch (no .swarm/ directory yet)
    if tui::onboarding::needs_onboarding(&work_dir) {
        let repos = core::git::detect_repos(&work_dir).unwrap_or_default();
        match tui::onboarding::show(&work_dir, &repos).await? {
            tui::onboarding::OnboardingResult::Launch => {} // continue to TUI
            tui::onboarding::OnboardingResult::Quit => return Ok(()),
        }
    }

    if !daemon::lifecycle::is_daemon_running(&work_dir) {
        // TUI has its own reconnect loop, so just spawn — don't block on readiness.
        daemon::lifecycle::spawn_daemon(&work_dir);
    } else {
        // Daemon already running — register workspace in background (don't block TUI startup)
        let bg_dir = work_dir.clone();
        tokio::task::spawn_blocking(move || {
            let _ = daemon::ipc_client::send_daemon_request(
                &bg_dir,
                &daemon::protocol::DaemonRequest::RegisterWorkspace {
                    path: bg_dir.clone(),
                },
            );
        });
    }

    daemon_tui::run(work_dir).await
}

/// Debug command: spawn claude via SDK (same code path as daemon) and print events.
/// Incrementally adds daemon infrastructure to isolate what breaks child spawning.
async fn cmd_debug_spawn(work_dir: std::path::PathBuf) -> Result<()> {
    tracing::debug!("Step 1: init logging");
    let _log_guard = core::log::init(&work_dir);

    tracing::debug!("Step 2: skip global env mutation (child spawn clears CLAUDECODE)");

    tracing::debug!("Step 3: ignore SIGPIPE");
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    tracing::debug!("Step 4: set up signal handlers");
    let _sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let _sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    tracing::debug!("Step 5: create channels");
    let (event_tx, _) = tokio::sync::broadcast::channel::<daemon::protocol::DaemonResponse>(1024);
    let (_supervisor_tx, _supervisor_rx) =
        tokio::sync::mpsc::unbounded_channel::<daemon::agent_supervisor::SupervisorEvent>();

    tracing::debug!("Step 6: start socket server");
    let (_request_rx, _socket_handle) = daemon::socket_server::start(event_tx.clone(), None, None)?;

    // Use an existing worktree directory if one exists, like the daemon does
    let wt_dir = std::fs::read_dir(work_dir.join(".swarm/wt"))
        .ok()
        .and_then(|mut rd| rd.find_map(|e| e.ok().map(|e| e.path())))
        .unwrap_or(work_dir);
    tracing::debug!(working_dir = %wt_dir.display(), "Step 7: spawn claude via SDK inside tokio::spawn");
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
                tracing::error!(error = %e, "Spawn failed");
                return 0u64;
            }
        };

        tracing::debug!("Spawned. Sending message...");

        if let Err(e) = session
            .send_message("Say hello in exactly 3 words. Nothing else.")
            .await
        {
            tracing::error!(error = %e, "Send failed");
            return 0;
        }

        tracing::debug!("Message sent. Reading events (stdin kept open)...");

        let mut count = 0u64;
        loop {
            match session.next_event().await {
                Ok(Some(event)) => {
                    count += 1;
                    let is_result = event.is_result();
                    tracing::debug!(count, "Event received");
                    if is_result {
                        break;
                    }
                }
                Ok(None) => {
                    tracing::debug!(count, "EOF");
                    break;
                }
                Err(e) => {
                    tracing::error!(count, error = %e, "Session error");
                    break;
                }
            }
        }
        count
    });

    let result = tokio::time::timeout(std::time::Duration::from_secs(15), handle).await;
    match result {
        Ok(Ok(count)) => tracing::debug!(count, "debug-spawn complete"),
        Ok(Err(e)) => tracing::error!(error = ?e, "debug-spawn panicked"),
        Err(_) => tracing::error!("debug-spawn timed out after 15s"),
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
            tracing::info!(path = %path.display(), "Reading prompt from file");
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
            tracing::info!(bytes = content.len(), "Loaded prompt from file");
            Ok(content)
        }
        (Some(prompt), None) => Ok(prompt),
        (None, None) => Err(color_eyre::eyre::eyre!(
            "either a positional <PROMPT> or --prompt-file is required"
        )),
    }
}

#[allow(clippy::too_many_arguments)]
async fn cmd_create(
    work_dir: std::path::PathBuf,
    prompt: Option<String>,
    prompt_file: Option<String>,
    agent: String,
    repo: Option<String>,
    modifiers: Vec<String>,
    profile: String,
    task_dir_path: Option<String>,
    role: String,
    review_pr: Option<u64>,
    base_branch: String,
) -> Result<()> {
    // Check prerequisites before doing any work
    if let Err(msg) = core::prerequisites::check_prerequisites() {
        return Err(color_eyre::eyre::eyre!("{}", msg));
    }

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

    daemon::lifecycle::ensure_daemon_running(&work_dir).await?;

    // Register this workspace first (idempotent)
    let _ = daemon::ipc_client::send_daemon_request(
        &work_dir,
        &daemon::protocol::DaemonRequest::RegisterWorkspace {
            path: work_dir.clone(),
        },
    );

    // Load task_dir payload from JSON file if provided
    let task_dir = if let Some(ref path) = task_dir_path {
        let content = std::fs::read_to_string(path).map_err(|e| {
            color_eyre::eyre::eyre!("failed to read task-dir file '{}': {}", path, e)
        })?;
        let payload: daemon::protocol::TaskDirPayload =
            serde_json::from_str(&content).map_err(|e| {
                color_eyre::eyre::eyre!("failed to parse task-dir JSON '{}': {}", path, e)
            })?;
        Some(payload)
    } else {
        None
    };

    let req = daemon::protocol::DaemonRequest::CreateWorker {
        prompt,
        agent,
        repo,
        start_point: None,
        workspace: Some(work_dir.clone()),
        profile: Some(profile),
        task_dir,
        role: Some(role),
        review_pr,
        base_branch: Some(base_branch),
    };
    match daemon::ipc_client::send_daemon_request(&work_dir, &req) {
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
    daemon::lifecycle::ensure_daemon_running(&work_dir).await?;
    let req = daemon::protocol::DaemonRequest::SendMessage {
        worktree_id: worktree,
        message,
    };
    match daemon::ipc_client::send_daemon_request(&work_dir, &req) {
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
    daemon::lifecycle::ensure_daemon_running(&work_dir).await?;
    let req = daemon::protocol::DaemonRequest::CloseWorker {
        worktree_id: worktree,
    };
    match daemon::ipc_client::send_daemon_request(&work_dir, &req) {
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
    daemon::lifecycle::ensure_daemon_running(&work_dir).await?;
    let req = daemon::protocol::DaemonRequest::MergeWorker {
        worktree_id: worktree,
    };
    match daemon::ipc_client::send_daemon_request(&work_dir, &req) {
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

/// Parse the git remote URL to get "owner/repo" for `gh` commands.
fn resolve_gh_nwo(repo_path: &std::path::Path) -> Result<String> {
    let output = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(repo_path)
        .output()
        .map_err(|e| color_eyre::eyre::eyre!("failed to run git remote get-url: {}", e))?;
    if !output.status.success() {
        return Err(color_eyre::eyre::eyre!("git remote get-url origin failed"));
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    parse_gh_nwo(&url)
}

/// Extract "owner/repo" from a GitHub remote URL.
///
/// Supported formats:
/// - `git@github.com:Owner/Repo.git` (SCP-style SSH)
/// - `ssh://git@github.com/Owner/Repo.git` (SSH URL)
/// - `https://github.com/Owner/Repo.git` (HTTPS)
fn parse_gh_nwo(url: &str) -> Result<String> {
    // SCP-style SSH: "git@github.com:Owner/Repo.git"
    if let Some(rest) = url.strip_prefix("git@github.com:") {
        return Ok(rest.trim_end_matches(".git").to_string());
    }
    // SSH URL: "ssh://git@github.com/Owner/Repo.git"
    if let Some(rest) = url.strip_prefix("ssh://git@github.com/") {
        return Ok(rest.trim_end_matches(".git").to_string());
    }
    // HTTPS: "https://github.com/Owner/Repo.git"
    if let Some(rest) = url.strip_prefix("https://github.com/") {
        return Ok(rest.trim_end_matches(".git").to_string());
    }
    Err(color_eyre::eyre::eyre!(
        "cannot parse GitHub owner/repo from remote URL: {}",
        url
    ))
}

#[allow(clippy::too_many_arguments)]
async fn cmd_adopt(
    work_dir: std::path::PathBuf,
    pr_number: u64,
    prompt: Option<String>,
    prompt_file: Option<String>,
    agent: String,
    repo: Option<String>,
    profile: String,
) -> Result<()> {
    if let Err(msg) = core::prerequisites::check_prerequisites() {
        return Err(color_eyre::eyre::eyre!("{}", msg));
    }

    // Resolve repo path (same logic as cmd_create)
    let repo_path = if let Some(ref name) = repo {
        let repos = core::git::detect_repos(&work_dir)?;
        repos
            .iter()
            .find(|r| core::git::repo_name(r) == *name)
            .cloned()
            .ok_or_else(|| color_eyre::eyre::eyre!("unknown repo '{}'", name))?
    } else {
        let repos = core::git::detect_repos(&work_dir)?;
        if repos.len() > 1 {
            let names: Vec<_> = repos.iter().map(|r| core::git::repo_name(r)).collect();
            return Err(color_eyre::eyre::eyre!(
                "multiple repos detected, --repo required: {}",
                names.join(", ")
            ));
        }
        repos
            .into_iter()
            .next()
            .ok_or_else(|| color_eyre::eyre::eyre!("no git repos detected"))?
    };

    // Get owner/repo for gh commands
    let nwo = resolve_gh_nwo(&repo_path)?;

    // Fetch PR details via gh
    let gh_output = std::process::Command::new("gh")
        .args([
            "pr",
            "view",
            &pr_number.to_string(),
            "--repo",
            &nwo,
            "--json",
            "headRefName,title,body,url",
        ])
        .output()
        .map_err(|e| color_eyre::eyre::eyre!("failed to run gh pr view: {}", e))?;
    if !gh_output.status.success() {
        let stderr = String::from_utf8_lossy(&gh_output.stderr);
        return Err(color_eyre::eyre::eyre!(
            "gh pr view failed: {}",
            stderr.trim()
        ));
    }
    let pr_json: serde_json::Value = serde_json::from_slice(&gh_output.stdout)
        .map_err(|e| color_eyre::eyre::eyre!("failed to parse gh pr view output: {}", e))?;
    let head_ref = pr_json["headRefName"]
        .as_str()
        .ok_or_else(|| color_eyre::eyre::eyre!("missing headRefName in PR data"))?
        .to_string();
    let pr_title = pr_json["title"].as_str().unwrap_or("").to_string();
    let pr_body = pr_json["body"].as_str().unwrap_or("").to_string();
    let pr_url = pr_json["url"].as_str().unwrap_or("").to_string();

    // Fetch the branch
    let fetch_status = std::process::Command::new("git")
        .args(["fetch", "origin", &head_ref])
        .current_dir(&repo_path)
        .status()
        .map_err(|e| color_eyre::eyre::eyre!("failed to run git fetch: {}", e))?;
    if !fetch_status.success() {
        return Err(color_eyre::eyre::eyre!(
            "git fetch origin {} failed",
            head_ref
        ));
    }

    // Build prompt
    let has_user_prompt = prompt.is_some() || prompt_file.is_some();
    let final_prompt = if has_user_prompt {
        let user_prompt = resolve_prompt(prompt, prompt_file)?;
        format!(
            "You are continuing work on PR #{}: {}\nPR URL: {}\n\n{}",
            pr_number, pr_title, pr_url, user_prompt
        )
    } else {
        format!(
            "You are continuing work on PR #{}: {}\n\n{}\n\nPR URL: {}\n\nContinue working on this PR. Review the existing changes and complete any remaining work.",
            pr_number, pr_title, pr_body, pr_url
        )
    };

    daemon::lifecycle::ensure_daemon_running(&work_dir).await?;

    // Register workspace
    let _ = daemon::ipc_client::send_daemon_request(
        &work_dir,
        &daemon::protocol::DaemonRequest::RegisterWorkspace {
            path: work_dir.clone(),
        },
    );

    // Create worker with start_point set to the PR branch
    let req = daemon::protocol::DaemonRequest::CreateWorker {
        prompt: final_prompt,
        agent,
        repo: repo.clone(),
        start_point: Some(format!("origin/{}", head_ref)),
        workspace: Some(work_dir.clone()),
        profile: Some(profile),
        task_dir: None,
        role: Some("worker".to_string()),
        review_pr: None,
        base_branch: Some("main".to_string()),
    };

    let wt_id = match daemon::ipc_client::send_daemon_request(&work_dir, &req) {
        Ok(daemon::protocol::DaemonResponse::Ok { data }) => {
            if let Some(data) = data
                && let Some(wt_id) = data.get("worktree_id").and_then(|v| v.as_str())
            {
                println!("{}", wt_id);
                Some(wt_id.to_string())
            } else {
                println!("created");
                None
            }
        }
        Ok(daemon::protocol::DaemonResponse::Error { message }) => {
            return Err(color_eyre::eyre::eyre!("{}", message));
        }
        Ok(_) => {
            println!("created");
            None
        }
        Err(e) => {
            return Err(color_eyre::eyre::eyre!("daemon request failed: {}", e));
        }
    };

    // Update state with PR info
    if let Some(wt_id) = wt_id {
        match core::state::load_state(&work_dir) {
            Ok(Some(mut state)) => {
                if let Some(wt) = state.worktrees.iter_mut().find(|w| w.id == wt_id) {
                    wt.pr = Some(core::state::PrInfo {
                        number: pr_number,
                        title: pr_title,
                        state: "OPEN".to_string(),
                        url: pr_url,
                    });
                    if let Err(e) = core::state::save_state(&work_dir, &state) {
                        tracing::warn!("failed to save PR info to state: {}", e);
                    }
                } else {
                    tracing::warn!("worktree {} not found in state after creation", wt_id);
                }
            }
            Ok(None) => {
                tracing::warn!("no state file found after creating worktree {}", wt_id);
            }
            Err(e) => {
                tracing::warn!("failed to load state for PR info update: {}", e);
            }
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

    #[test]
    fn parse_gh_nwo_scp_ssh() {
        let result = parse_gh_nwo("git@github.com:Owner/Repo.git").unwrap();
        assert_eq!(result, "Owner/Repo");
    }

    #[test]
    fn parse_gh_nwo_scp_ssh_no_dot_git() {
        let result = parse_gh_nwo("git@github.com:Owner/Repo").unwrap();
        assert_eq!(result, "Owner/Repo");
    }

    #[test]
    fn parse_gh_nwo_https() {
        let result = parse_gh_nwo("https://github.com/Owner/Repo.git").unwrap();
        assert_eq!(result, "Owner/Repo");
    }

    #[test]
    fn parse_gh_nwo_https_no_dot_git() {
        let result = parse_gh_nwo("https://github.com/Owner/Repo").unwrap();
        assert_eq!(result, "Owner/Repo");
    }

    #[test]
    fn parse_gh_nwo_ssh_url() {
        let result = parse_gh_nwo("ssh://git@github.com/Owner/Repo.git").unwrap();
        assert_eq!(result, "Owner/Repo");
    }

    #[test]
    fn parse_gh_nwo_unknown_host() {
        let err = parse_gh_nwo("git@gitlab.com:Owner/Repo.git").unwrap_err();
        assert!(err.to_string().contains("cannot parse"));
    }
}
