mod agent_tui;
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

    /// Agent to use: claude-tui, claude, codex
    #[arg(short, long, default_value = "claude-tui", global = true)]
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
    /// Show PR details in a tmux popup
    PrPopup {
        /// PR number
        #[arg(long)]
        number: u64,
        /// PR title
        #[arg(long)]
        title: String,
        /// PR state (OPEN, MERGED, CLOSED)
        #[arg(long)]
        state: String,
        /// PR URL
        #[arg(long)]
        url: String,
    },
    /// Run the TUI-native Claude agent (launched inside a tmux pane)
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
        Some(Commands::Create {
            prompt,
            prompt_file,
            agent,
            repo,
        }) => cmd_create(
            work_dir,
            prompt,
            prompt_file,
            agent.unwrap_or_else(|| cli.agent.clone()),
            repo,
        ),
        Some(Commands::Send { worktree, message }) => cmd_send(work_dir, worktree, message),
        Some(Commands::Close { worktree }) => cmd_close(work_dir, worktree),
        Some(Commands::Merge { worktree }) => cmd_merge(work_dir, worktree),
        Some(Commands::Pick) => {
            let repos = core::git::detect_repos(&work_dir)?;
            tui::picker::run_picker(work_dir, repos)
        }
        Some(Commands::PrPopup {
            number,
            title,
            state,
            url,
        }) => tui::pr_popup::run_pr_popup(tui::pr_popup::PrPopupArgs {
            number,
            title,
            state,
            url,
        }),
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
                // Keep sidebar bright even when window-style defaults to dimmed
                let _ = core::tmux::set_pane_style(pane_id, "bg=#302c26,fg=#dcdce1,nodim");
            }
            app.save_state();
            // Enforce correct layout sizes (sidebar may have drifted if
            // terminal was resized while detached, e.g. mobile SSH).
            app.rebalance_layout();
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
            rebalance_session(&session_name);
            core::tmux::switch_client(&session_name)?;
        }
    } else {
        // Not inside tmux
        if !core::tmux::session_exists(&session_name) {
            let cmd = build_swarm_cmd(&work_dir, &agent);
            core::tmux::create_session_with_cmd(&session_name, &work_dir.to_string_lossy(), &cmd)?;
            core::tmux::apply_session_style(&session_name)?;
        }
        rebalance_session(&session_name);
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

/// Rebalance the tmux layout for an existing swarm session.
/// Reads state.json to find the sidebar and worktree panes, then applies
/// the tiled layout with 38-char sidebar. Called before attach/switch so
/// the layout is correct even if the terminal size changed while detached.
fn rebalance_session(session_name: &str) {
    let work_dir = std::env::current_dir().unwrap_or_default();
    let state = match core::state::load_state(&work_dir) {
        Ok(Some(s)) if s.session_name == *session_name => s,
        _ => return,
    };

    let sidebar = match &state.sidebar_pane_id {
        Some(id) => id.clone(),
        None => return,
    };

    let live_panes: Vec<String> = core::tmux::list_panes(session_name)
        .unwrap_or_default()
        .iter()
        .map(|p| p.pane_id.clone())
        .collect();

    let pane_groups: Vec<Vec<String>> = state
        .worktrees
        .iter()
        .map(|wt| {
            let mut panes = Vec::new();
            if let Some(ref agent) = wt.agent {
                if live_panes.contains(&agent.pane_id) {
                    panes.push(agent.pane_id.clone());
                }
            }
            for term in &wt.terminals {
                if live_panes.contains(&term.pane_id) {
                    panes.push(term.pane_id.clone());
                }
            }
            panes
        })
        .collect();

    let _ = core::tmux::apply_tiled_layout(session_name, &sidebar, 38, pane_groups);
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

/// Ensure the swarm tmux session is running, starting it (detached) if not.
fn ensure_swarm_running(work_dir: &std::path::Path, agent: &str) -> Result<()> {
    if !core::tmux::has_tmux() {
        return Err(color_eyre::eyre::eyre!("tmux is not installed"));
    }

    let dir_name = work_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "swarm".to_string());
    let session_name = format!("swarm-{}", dir_name);

    if !core::tmux::session_exists(&session_name) {
        eprintln!("[swarm] Starting swarm session: {session_name}");
        let cmd = build_swarm_cmd(work_dir, agent);
        core::tmux::create_session_with_cmd(&session_name, &work_dir.to_string_lossy(), &cmd)?;
        core::tmux::apply_session_style(&session_name)?;
    }

    Ok(())
}

// ── IPC Subcommands ────────────────────────────────────────

fn cmd_status(work_dir: std::path::PathBuf, json: bool) -> Result<()> {
    let state = core::state::load_state(&work_dir)?;
    match state {
        Some(mut s) => {
            if json {
                // Check tmux pane liveness to compute accurate status
                let live_panes = live_pane_ids(&s.session_name);
                for wt in &mut s.worktrees {
                    wt.status = worktree_status(wt, &live_panes);
                }
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

/// Get all live tmux pane IDs for a session.
fn live_pane_ids(session: &str) -> Vec<String> {
    // Use -s to list panes across all windows in the session
    std::process::Command::new("tmux")
        .args(["list-panes", "-s", "-t", session, "-F", "#{pane_id}"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Determine worktree status by checking if any of its panes are still live.
fn worktree_status(wt: &core::state::WorktreeState, live_panes: &[String]) -> String {
    let agent_alive = wt
        .agent
        .as_ref()
        .is_some_and(|a| live_panes.contains(&a.pane_id));
    let term_alive = wt
        .terminals
        .iter()
        .any(|t| live_panes.contains(&t.pane_id));
    if agent_alive || term_alive {
        "running".to_string()
    } else {
        "done".to_string()
    }
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
            eprintln!(
                "[swarm] loaded prompt from file ({} bytes)",
                content.len()
            );
            Ok(content)
        }
        (Some(prompt), None) => Ok(prompt),
        (None, None) => Err(color_eyre::eyre::eyre!(
            "either a positional <PROMPT> or --prompt-file is required"
        )),
    }
}

fn cmd_create(
    work_dir: std::path::PathBuf,
    prompt: Option<String>,
    prompt_file: Option<String>,
    agent: String,
    repo: Option<String>,
) -> Result<()> {
    let prompt = resolve_prompt(prompt, prompt_file)?;

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

    ensure_swarm_running(&work_dir, &agent)?;

    let msg = core::ipc::InboxMessage::Create {
        id: Uuid::new_v4().to_string(),
        prompt,
        agent,
        repo,
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
