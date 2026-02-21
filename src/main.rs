mod core;
mod tui;

use chrono::Local;
use clap::{Parser, Subcommand};
use color_eyre::Result;
use uuid::Uuid;

#[derive(Parser)]
#[command(name = "swarm", version, about = "Run agents in parallel.")]
struct Cli {
    /// Working directory (defaults to current dir)
    #[arg(short, long, global = true)]
    dir: Option<String>,

    /// Agent to use: claude, codex, opencode
    #[arg(short, long, default_value = "claude", global = true)]
    agent: String,

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
    /// Create a new worktree + agent via IPC
    Create {
        /// Task prompt
        prompt: String,
        /// Agent type
        #[arg(long, default_value = "claude")]
        agent: Option<String>,
    },
    /// Send a message to a worktree's agent via IPC
    Send {
        /// Worktree ID
        worktree: String,
        /// Message to send
        message: String,
    },
    /// Close a worktree via IPC
    Close {
        /// Worktree ID
        worktree: String,
    },
    /// Merge a worktree via IPC
    Merge {
        /// Worktree ID
        worktree: String,
    },
    /// Interactive picker for new worktree (runs inside tmux popup)
    Pick,
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
        None => run_sidebar(work_dir, cli.agent).await,
        Some(Commands::Status { json }) => cmd_status(work_dir, json),
        Some(Commands::Create { prompt, agent }) => {
            cmd_create(work_dir, prompt, agent.unwrap_or_else(|| cli.agent.clone()))
        }
        Some(Commands::Send { worktree, message }) => cmd_send(work_dir, worktree, message),
        Some(Commands::Close { worktree }) => cmd_close(work_dir, worktree),
        Some(Commands::Merge { worktree }) => cmd_merge(work_dir, worktree),
        Some(Commands::Pick) => {
            let repos = core::git::detect_repos(&work_dir)?;
            tui::picker::run_picker(work_dir, repos)
        }
    }
}

/// Default command: create/join tmux session, run sidebar TUI.
async fn run_sidebar(work_dir: std::path::PathBuf, agent: String) -> Result<()> {
    let dir_name = work_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "swarm".to_string());
    let session_name = format!("swarm-{}", dir_name);

    if !core::tmux::has_tmux() {
        return Err(color_eyre::eyre::eyre!("tmux is not installed"));
    }

    if core::tmux::inside_tmux() {
        let current = core::tmux::current_session().unwrap_or_default();
        if current == session_name {
            // We're in the right session — run TUI directly (this IS the sidebar)
            let mut app = tui::app::App::new(work_dir, agent)?;
            app.sidebar_pane_id = get_current_pane_id();
            if let Some(ref pane_id) = app.sidebar_pane_id {
                let _ = core::tmux::set_pane_title(pane_id, "swarm");
                // Mark as sidebar so pane-border-format renders no title for it
                let _ = std::process::Command::new("tmux")
                    .args(["set-option", "-p", "-t", pane_id, "@sidebar", "1"])
                    .output();
            }
            app.save_state();
            tui::run(&mut app).await?;
        } else {
            // In a different tmux session — create swarm session if needed, switch to it
            if !core::tmux::session_exists(&session_name) {
                let cmd = build_swarm_cmd(&work_dir, &agent);
                core::tmux::create_session_with_cmd(
                    &session_name,
                    &work_dir.to_string_lossy(),
                    &cmd,
                )?;
                core::tmux::apply_session_style(&session_name)?;
            }
            core::tmux::switch_client(&session_name)?;
        }
    } else {
        // Not inside tmux
        if !core::tmux::session_exists(&session_name) {
            let cmd = build_swarm_cmd(&work_dir, &agent);
            core::tmux::create_session_with_cmd(
                &session_name,
                &work_dir.to_string_lossy(),
                &cmd,
            )?;
            core::tmux::apply_session_style(&session_name)?;
        }
        core::tmux::attach_session(&session_name)?;
    }

    Ok(())
}

/// Get the current pane ID (e.g. "%0") from inside tmux.
fn get_current_pane_id() -> Option<String> {
    std::process::Command::new("tmux")
        .args(["display-message", "-p", "#{pane_id}"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
}

/// Build the swarm command to send into a tmux pane.
/// Uses the full binary path since tmux sessions may not have ~/.cargo/bin in PATH.
fn build_swarm_cmd(work_dir: &std::path::Path, agent: &str) -> String {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "swarm".to_string());
    let dir_str = work_dir.to_string_lossy();
    format!("'{}' -d '{}' -a '{}'", exe, dir_str, agent)
}

// ── IPC Subcommands ────────────────────────────────────────

fn cmd_status(work_dir: std::path::PathBuf, json: bool) -> Result<()> {
    let state = core::state::load_state(&work_dir)?;
    match state {
        Some(s) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&s)?);
            } else {
                println!("session: {}", s.session_name);
                if let Some(ref sid) = s.sidebar_pane_id {
                    println!("sidebar: {}", sid);
                }
                println!("worktrees: {}", s.worktrees.len());
                for wt in &s.worktrees {
                    let agent_info = wt
                        .agent
                        .as_ref()
                        .map(|a| format!(" (agent: {})", a.pane_id))
                        .unwrap_or_default();
                    println!(
                        "  {} [{}] {}{}",
                        wt.id,
                        wt.agent_kind.label(),
                        wt.branch,
                        agent_info
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

fn cmd_create(work_dir: std::path::PathBuf, prompt: String, agent: String) -> Result<()> {
    let msg = core::ipc::InboxMessage::Create {
        id: Uuid::new_v4().to_string(),
        prompt,
        agent,
        repo: None,
        start_point: None,
        timestamp: Local::now(),
    };
    core::ipc::write_inbox(&work_dir, &msg)?;
    println!("queued create");
    Ok(())
}

fn cmd_send(work_dir: std::path::PathBuf, worktree: String, message: String) -> Result<()> {
    let msg = core::ipc::InboxMessage::Send {
        id: Uuid::new_v4().to_string(),
        worktree,
        message,
        timestamp: Local::now(),
    };
    core::ipc::write_inbox(&work_dir, &msg)?;
    println!("queued send");
    Ok(())
}

fn cmd_close(work_dir: std::path::PathBuf, worktree: String) -> Result<()> {
    let msg = core::ipc::InboxMessage::Close {
        id: Uuid::new_v4().to_string(),
        worktree,
        timestamp: Local::now(),
    };
    core::ipc::write_inbox(&work_dir, &msg)?;
    println!("queued close");
    Ok(())
}

fn cmd_merge(work_dir: std::path::PathBuf, worktree: String) -> Result<()> {
    let msg = core::ipc::InboxMessage::Merge {
        id: Uuid::new_v4().to_string(),
        worktree,
        timestamp: Local::now(),
    };
    core::ipc::write_inbox(&work_dir, &msg)?;
    println!("queued merge");
    Ok(())
}
