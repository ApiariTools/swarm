/// Write a diagnostic line to `.swarm/tui-debug.log` (TUI process can't use
/// eprintln because stdout is in raw mode, and tracing is only initialised
/// in the daemon process).
macro_rules! tui_log {
    ($work_dir:expr, $($arg:tt)*) => {{
        use std::io::Write;
        let ts = chrono::Local::now().format("%H:%M:%S%.3f");
        let msg = format!("[{}] {}\n", ts, format!($($arg)*));
        let path = $work_dir.join(".swarm").join("tui-debug.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = f.write_all(msg.as_bytes());
        }
    }};
}

pub mod app;
pub mod render;
pub mod socket_client;

use app::{DaemonTuiApp, Mode, Panel, PendingAction, PrDetailInfo, daemon_agents};
use color_eyre::Result;
use crossterm::ExecutableCommand;
use crossterm::event::{EventStream, KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::prelude::*;
use socket_client::{DaemonClient, DaemonReader};
use std::io::stdout;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::core::modifier;
use crate::daemon::protocol::{DaemonRequest, DaemonResponse};

/// Run the daemon TUI connected to a remote daemon via TCP.
pub async fn run_remote(addr: String, token: String) -> Result<()> {
    let mut client = DaemonClient::connect_tcp(&addr).await?;

    // Authenticate
    if !token.is_empty() {
        client
            .send(&DaemonRequest::Auth {
                token: token.clone(),
            })
            .await?;
        let auth_resp = client.next_response().await?;
        match auth_resp {
            DaemonResponse::Ok { .. } => {}
            DaemonResponse::Error { message } => {
                return Err(color_eyre::eyre::eyre!("auth failed: {}", message));
            }
            _ => {}
        }
    }

    // Subscribe to all worker events
    let sub_resp = client.subscribe(None).await?;
    match sub_resp {
        DaemonResponse::Ok { .. } => {}
        DaemonResponse::Error { message } => {
            return Err(color_eyre::eyre::eyre!("subscribe failed: {}", message));
        }
        _ => {}
    }

    let mut app = DaemonTuiApp::new(PathBuf::from("."));
    app.connected = true;
    app.is_remote = true;

    // Fire-and-forget: request worker list. Response arrives via reader task.
    if let Err(e) = client
        .send(&DaemonRequest::ListWorkers { workspace: None })
        .await
    {
        app.set_status(format!("list workers failed: {}", e));
    }

    // Enter terminal raw mode
    stdout().execute(EnterAlternateScreen)?;
    stdout().execute(crossterm::event::EnableMouseCapture)?;
    enable_raw_mode()?;

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    terminal.clear()?;

    let result = event_loop(&mut terminal, &mut app, &mut client, None).await;

    disable_raw_mode()?;
    stdout().execute(crossterm::event::DisableMouseCapture)?;
    stdout().execute(LeaveAlternateScreen)?;

    result
}

/// Run the daemon TUI.
pub async fn run(work_dir: PathBuf) -> Result<()> {
    tui_log!(&work_dir, "run() entry");

    // Enter terminal raw mode FIRST — user sees TUI instantly
    stdout().execute(EnterAlternateScreen)?;
    stdout().execute(crossterm::event::EnableMouseCapture)?;
    enable_raw_mode()?;
    tui_log!(&work_dir, "TUI visible (EnterAlternateScreen)");

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    terminal.clear()?;

    // App starts disconnected; reconnect loop handles initial connection.
    // Set reconnect_at to the past so the first tick triggers an immediate attempt.
    let mut app = DaemonTuiApp::new(work_dir.clone());
    app.reconnect_at = Some(std::time::Instant::now() - Duration::from_secs(1));

    // Create a placeholder client (not yet connected)
    let mut client = DaemonClient::disconnected();

    // First draw — shows "connecting..." state
    terminal.draw(|frame| render::draw(frame, &mut app))?;
    app.needs_redraw = false;
    tui_log!(&work_dir, "first draw complete");

    // Detect repos in background (spawns git subprocesses, can take seconds)
    let bg_work_dir = work_dir.clone();
    let repo_task = tokio::task::spawn_blocking(move || {
        crate::core::git::detect_repos(&bg_work_dir).unwrap_or_default()
    });

    let result = event_loop(&mut terminal, &mut app, &mut client, Some(repo_task)).await;

    disable_raw_mode()?;
    stdout().execute(crossterm::event::DisableMouseCapture)?;
    stdout().execute(LeaveAlternateScreen)?;

    result
}

// ── Background reader task ──────────────────────────────────

/// Background task that reads daemon responses and forwards them on a channel.
///
/// Runs `read_line` in a tight loop (no `select!`, no timeouts) so there are
/// no cancellation-safety issues. Exits on EOF, I/O error, or when the
/// receiver is dropped.
async fn daemon_reader_task(
    mut reader: DaemonReader,
    tx: mpsc::UnboundedSender<Result<DaemonResponse>>,
) {
    loop {
        let result = reader.next_response().await;
        let is_err = result.is_err();
        if tx.send(result).is_err() {
            break; // receiver dropped
        }
        if is_err {
            break; // disconnected
        }
    }
}

// ── Main event loop ─────────────────────────────────────────

/// The main TUI event loop using `tokio::select!` with three async sources:
/// 1. `EventStream` — crossterm terminal events (keyboard, mouse)
/// 2. `mpsc channel` — daemon responses from the reader task
/// 3. `interval` — periodic tick for animations and housekeeping
async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut DaemonTuiApp,
    client: &mut DaemonClient,
    mut repo_task: Option<tokio::task::JoinHandle<Vec<PathBuf>>>,
) -> Result<()> {
    // Track consecutive reconnect failures for daemon health detection
    let mut reconnect_failures: u32 = 0;

    // Track reader task so we can abort it on reconnect (prevents stale
    // EOF errors from the old connection triggering unnecessary reconnects).
    let mut reader_task: Option<tokio::task::JoinHandle<()>> = None;

    // Spawn daemon reader task
    let (daemon_tx, mut daemon_rx) = mpsc::unbounded_channel();
    if let Some(reader) = client.take_reader() {
        reader_task = Some(tokio::spawn(daemon_reader_task(reader, daemon_tx.clone())));
    }

    // Last time we sent a ListWorkers request (for periodic refresh)
    let mut last_worker_request = std::time::Instant::now();

    // Async terminal event stream (non-blocking, backed by internal thread)
    let mut event_stream = EventStream::new();

    // Tick interval for spinner animation and periodic housekeeping
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut draw_count: u32 = 0;
    loop {
        // Draw if state has changed
        if app.needs_redraw {
            terminal.draw(|frame| render::draw(frame, app))?;
            app.needs_redraw = false;
            draw_count += 1;
            // Log first few draws with state summary for diagnostics
            if draw_count <= 5 {
                let sel_id = app
                    .selected_worker()
                    .map(|w| w.id.as_str())
                    .unwrap_or("none");
                let conv_entries = app
                    .selected_conversation()
                    .map(|c| c.entries.len())
                    .unwrap_or(0);
                let streaming = app
                    .selected_conversation()
                    .map(|c| c.streaming_text.len())
                    .unwrap_or(0);
                tui_log!(
                    &app.work_dir,
                    "draw #{}: connected={} workers={} selected={} entries={} streaming_bytes={}",
                    draw_count,
                    app.connected,
                    app.workers.len(),
                    sel_id,
                    conv_entries,
                    streaming
                );
            }
        }

        // Lazy-load history for the selected worker on first view
        if let Some(w) = app.selected_worker() {
            let wt_id = w.id.clone();
            if app.connected && !app.history_loaded.contains(&wt_id) {
                tui_log!(&app.work_dir, "requesting history for {}", wt_id);
                app.history_loaded.insert(wt_id.clone());
                app.pending_history.push_back(wt_id.clone());
                if let Err(e) = client
                    .send(&DaemonRequest::GetHistory { worktree_id: wt_id })
                    .await
                {
                    app.pending_history.pop_back();
                    handle_send_error(app, e);
                }
            }
        }

        tokio::select! {
            // Terminal events (keyboard, mouse, resize)
            maybe_event = event_stream.next() => {
                let event = match maybe_event {
                    Some(Ok(event)) => event,
                    Some(Err(_)) => continue,
                    None => break, // terminal stream ended
                };

                app.needs_redraw = true;
                let action = match event {
                    crossterm::event::Event::Key(key) => {
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            && key.code == KeyCode::Char('c')
                        {
                            break;
                        }
                        handle_key(app, key)
                    }
                    crossterm::event::Event::Mouse(mouse) => handle_mouse(app, mouse),
                    _ => KeyAction::None,
                };

                if matches!(action, KeyAction::Quit) {
                    break;
                }
                handle_action(app, client, action).await;
            }

            // Daemon responses from the background reader task
            Some(result) = daemon_rx.recv() => {
                match result {
                    Ok(resp) => {
                        // Detect new workers before updating state, so we can
                        // trigger an immediate PR poll for them.
                        let new_worker_ids = if let DaemonResponse::Workers { ref workers } = resp {
                            let existing: std::collections::HashSet<&str> =
                                app.workers.iter().map(|w| w.id.as_str()).collect();
                            workers
                                .iter()
                                .filter(|w| !existing.contains(w.id.as_str()))
                                .map(|w| w.id.clone())
                                .collect::<Vec<_>>()
                        } else {
                            Vec::new()
                        };

                        handle_daemon_response(app, resp);

                        if !new_worker_ids.is_empty() {
                            fire_and_forget(
                                client,
                                app,
                                DaemonRequest::TriggerPrPoll {
                                    worker_ids: new_worker_ids,
                                },
                            )
                            .await;
                        }
                    }
                    Err(e) => {
                        // Only trigger disconnect if the current reader task
                        // is the one reporting the error (not an aborted stale one).
                        let is_current = reader_task
                            .as_ref()
                            .is_some_and(|t| !t.is_finished());
                        if is_current || reader_task.is_none() {
                            tui_log!(&app.work_dir, "reader error (current): {}", e);
                            app.connected = false;
                            app.reconnect_at = Some(std::time::Instant::now());
                            app.set_status(format!("disconnected: {}", e));
                        } else {
                            tui_log!(&app.work_dir, "reader error (stale, ignoring): {}", e);
                        }
                    }
                }
                app.needs_redraw = true;
            }

            // Periodic tick for animations and housekeeping
            _ = tick.tick() => {
                app.tick();

                if let Some(conv) = app.selected_conversation_mut() {
                    conv.validate_focus();
                }

                // Check if background repo detection finished
                if let Some(ref task) = repo_task
                    && task.is_finished()
                    && let Some(task) = repo_task.take()
                    && let Ok(repos) = task.await
                {
                    tui_log!(&app.work_dir, "detect_repos finished: {} repos", repos.len());
                    app.repos = repos;
                    app.needs_redraw = true;
                }

                // Periodic worker refresh: every 5s while connected, so workers
                // created by other agents/processes appear automatically.
                if app.connected
                    && last_worker_request.elapsed() >= Duration::from_secs(5)
                {
                    last_worker_request = std::time::Instant::now();
                    fire_and_forget(
                        client,
                        app,
                        DaemonRequest::ListWorkers {
                            workspace: Some(app.work_dir.clone()),
                        },
                    )
                    .await;
                }

                // Reconnection attempts
                if !app.connected
                    && let Some(at) = app.reconnect_at
                    && at.elapsed() >= Duration::from_millis(500)
                {
                    app.reconnect_at = Some(std::time::Instant::now());
                    tui_log!(&app.work_dir, "reconnect attempt #{}", reconnect_failures + 1);
                    match try_reconnect(app, client, &daemon_tx, &mut reader_task).await {
                        Ok(()) => {
                            app.connected = true;
                            app.reconnect_at = None;
                            reconnect_failures = 0;
                            tui_log!(&app.work_dir, "connected + subscribed");
                            app.set_status("connected".to_string());
                        }
                        Err(e) => {
                            reconnect_failures += 1;
                            tui_log!(&app.work_dir, "reconnect failed ({}): {}", reconnect_failures, e);

                            // After 10 consecutive failures (~5s), the daemon is
                            // likely stuck. Restart it and reset the counter.
                            if reconnect_failures == 10 {
                                tui_log!(&app.work_dir, "daemon unresponsive, restarting...");
                                app.set_status("daemon unresponsive, restarting...".to_string());
                                restart_daemon(&app.work_dir);
                                reconnect_failures = 0;
                                // Give daemon time to start before retrying
                                app.reconnect_at = Some(
                                    std::time::Instant::now() + Duration::from_secs(1),
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Actions resulting from key presses.
#[derive(Debug)]
enum KeyAction {
    None,
    Quit,
    CreateWorker {
        prompt: String,
        agent: String,
        repo: Option<String>,
    },
    SendMessage {
        worktree_id: String,
        text: String,
    },
    CloseWorker(String),
    MergeWorker(String),
}

/// Execute a KeyAction by sending requests to the daemon.
async fn handle_action(app: &mut DaemonTuiApp, client: &mut DaemonClient, action: KeyAction) {
    match action {
        KeyAction::None | KeyAction::Quit => {}
        KeyAction::CreateWorker {
            prompt,
            agent,
            repo,
        } => {
            app.set_status("creating worker...".to_string());
            let req = DaemonRequest::CreateWorker {
                prompt,
                agent,
                repo,
                start_point: None,
                workspace: Some(app.work_dir.clone()),
                profile: None,
                task_dir: None,
                role: None,
                review_pr: None,
                base_branch: None,
            };
            fire_and_forget(client, app, req).await;
            fire_and_forget(
                client,
                app,
                DaemonRequest::ListWorkers {
                    workspace: Some(app.work_dir.clone()),
                },
            )
            .await;
        }
        KeyAction::SendMessage { worktree_id, text } => {
            let req = DaemonRequest::SendMessage {
                worktree_id: worktree_id.clone(),
                message: text.clone(),
            };
            // Add user message to conversation immediately
            if let Some(conv) = app.conversations.get_mut(&worktree_id) {
                conv.entries
                    .push(crate::agent_tui::app::ConversationEntry::User {
                        text,
                        timestamp: chrono::Local::now().format("%-I:%M %p").to_string(),
                    });
                conv.auto_scroll = true;
            }
            fire_and_forget(client, app, req).await;
        }
        KeyAction::CloseWorker(id) => {
            app.set_status(format!("closing {}...", id));
            let req = DaemonRequest::CloseWorker { worktree_id: id };
            fire_and_forget(client, app, req).await;
            fire_and_forget(
                client,
                app,
                DaemonRequest::ListWorkers {
                    workspace: Some(app.work_dir.clone()),
                },
            )
            .await;
        }
        KeyAction::MergeWorker(id) => {
            app.set_status(format!("merging {}...", id));
            let req = DaemonRequest::MergeWorker { worktree_id: id };
            fire_and_forget(client, app, req).await;
            fire_and_forget(
                client,
                app,
                DaemonRequest::ListWorkers {
                    workspace: Some(app.work_dir.clone()),
                },
            )
            .await;
        }
    }
}

/// Handle a key event and return the action to take.
fn handle_key(app: &mut DaemonTuiApp, key: crossterm::event::KeyEvent) -> KeyAction {
    match app.mode {
        Mode::Help => {
            // Any key dismisses help
            app.mode = Mode::Normal;
            KeyAction::None
        }
        Mode::PrDetail => match key.code {
            KeyCode::Char('o') | KeyCode::Enter => {
                if let Some(ref pr) = app.pr_detail {
                    let _ = std::process::Command::new("open").arg(&pr.url).spawn();
                }
                app.mode = Mode::Normal;
                app.pr_detail = None;
                KeyAction::None
            }
            KeyCode::Char('c') => {
                if let Some(ref pr) = app.pr_detail {
                    let _ = std::process::Command::new("sh")
                        .args(["-c", &format!("printf '%s' '{}' | pbcopy", pr.url)])
                        .status();
                    app.set_status("copied PR URL".to_string());
                }
                app.mode = Mode::Normal;
                app.pr_detail = None;
                KeyAction::None
            }
            KeyCode::Esc | KeyCode::Char('p') | KeyCode::Char('q') => {
                app.mode = Mode::Normal;
                app.pr_detail = None;
                KeyAction::None
            }
            _ => KeyAction::None,
        },
        Mode::Confirm => match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                app.mode = Mode::Normal;
                match app.pending_action.take() {
                    Some(PendingAction::Close(id)) => KeyAction::CloseWorker(id),
                    Some(PendingAction::Merge(id)) => KeyAction::MergeWorker(id),
                    None => KeyAction::None,
                }
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                app.mode = Mode::Normal;
                app.pending_action = None;
                KeyAction::None
            }
            _ => KeyAction::None,
        },
        Mode::CreatePrompt => match key.code {
            KeyCode::Enter => {
                let prompt = app.take_input();
                if prompt.trim().is_empty() {
                    app.mode = Mode::Normal;
                    return KeyAction::None;
                }
                app.pending_prompt = prompt;
                app.agent_select_index = 0;
                app.mode = Mode::AgentSelect;
                KeyAction::None
            }
            KeyCode::Esc => {
                app.mode = Mode::Normal;
                app.input_buffer.clear();
                app.input_cursor = 0;
                KeyAction::None
            }
            KeyCode::Backspace => {
                app.input_backspace();
                KeyAction::None
            }
            KeyCode::Left => {
                app.input_cursor_left();
                KeyAction::None
            }
            KeyCode::Right => {
                app.input_cursor_right();
                KeyAction::None
            }
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::ALT) && c == '\r' {
                    app.input_char('\n');
                } else {
                    app.input_char(c);
                }
                KeyAction::None
            }
            _ => KeyAction::None,
        },
        Mode::RepoSelect => match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                app.repo_select_index = app.repo_select_index.saturating_sub(1);
                KeyAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if app.repo_select_index + 1 < app.repos.len() {
                    app.repo_select_index += 1;
                }
                KeyAction::None
            }
            KeyCode::Enter => {
                app.pending_repo = app
                    .repos
                    .get(app.repo_select_index)
                    .map(|r| crate::core::git::repo_name(r));
                app.input_buffer.clear();
                app.input_cursor = 0;
                app.mode = Mode::CreatePrompt;
                KeyAction::None
            }
            KeyCode::Esc => {
                app.mode = Mode::Normal;
                KeyAction::None
            }
            _ => KeyAction::None,
        },
        Mode::AgentSelect => match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                app.agent_select_index = app.agent_select_index.saturating_sub(1);
                KeyAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let count = daemon_agents().len();
                if app.agent_select_index + 1 < count {
                    app.agent_select_index += 1;
                }
                KeyAction::None
            }
            KeyCode::Enter => {
                app.modifier_cursor = 0;
                app.modifier_selected = vec![false; app.modifier_prompts.len()];
                app.mode = Mode::ModifierSelect;
                KeyAction::None
            }
            KeyCode::Esc => {
                app.mode = Mode::CreatePrompt;
                KeyAction::None
            }
            // Number keys for quick selection
            KeyCode::Char(c @ '1'..='9') => {
                let idx = (c as usize) - ('1' as usize);
                let agents = daemon_agents();
                if idx < agents.len() {
                    app.agent_select_index = idx;
                    app.modifier_cursor = 0;
                    app.modifier_selected = vec![false; app.modifier_prompts.len()];
                    app.mode = Mode::ModifierSelect;
                }
                KeyAction::None
            }
            _ => KeyAction::None,
        },
        Mode::ModifierSelect => match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                app.modifier_cursor = app.modifier_cursor.saturating_sub(1);
                KeyAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if app.modifier_cursor + 1 < app.modifier_prompts.len() {
                    app.modifier_cursor += 1;
                }
                KeyAction::None
            }
            KeyCode::Char(' ') => {
                if let Some(sel) = app.modifier_selected.get_mut(app.modifier_cursor) {
                    *sel = !*sel;
                }
                KeyAction::None
            }
            KeyCode::Char('a') => {
                let any_selected = app.modifier_selected.iter().any(|&s| s);
                let new_val = !any_selected;
                for sel in &mut app.modifier_selected {
                    *sel = new_val;
                }
                KeyAction::None
            }
            KeyCode::Enter => {
                let agents = daemon_agents();
                let agent = agents
                    .get(app.agent_select_index)
                    .map(|a| a.label().to_string())
                    .unwrap_or_else(|| "claude".to_string());
                let raw_prompt = std::mem::take(&mut app.pending_prompt);
                let repo = app.pending_repo.take();

                // Assemble modifiers into prompt
                let prompt = modifier::assemble_prompt(
                    &raw_prompt,
                    &app.modifier_prompts,
                    &app.modifier_selected,
                );

                app.mode = Mode::Normal;
                KeyAction::CreateWorker {
                    prompt,
                    agent,
                    repo,
                }
            }
            KeyCode::Esc => {
                app.mode = Mode::AgentSelect;
                KeyAction::None
            }
            _ => KeyAction::None,
        },
        Mode::Input => match key.code {
            KeyCode::Enter => {
                let text = app.take_input();
                app.mode = Mode::Normal;
                if text.trim().is_empty() {
                    return KeyAction::None;
                }
                if let Some(worker) = app.selected_worker() {
                    let wt_id = worker.id.clone();
                    KeyAction::SendMessage {
                        worktree_id: wt_id,
                        text,
                    }
                } else {
                    KeyAction::None
                }
            }
            KeyCode::Esc => {
                app.mode = Mode::Normal;
                app.input_buffer.clear();
                app.input_cursor = 0;
                KeyAction::None
            }
            KeyCode::Backspace => {
                app.input_backspace();
                KeyAction::None
            }
            KeyCode::Left => {
                app.input_cursor_left();
                KeyAction::None
            }
            KeyCode::Right => {
                app.input_cursor_right();
                KeyAction::None
            }
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::ALT) && c == '\r' {
                    app.input_char('\n');
                } else {
                    app.input_char(c);
                }
                KeyAction::None
            }
            _ => KeyAction::None,
        },
        Mode::Normal => match app.focus {
            Panel::Sidebar => match key.code {
                KeyCode::Char('q') => KeyAction::Quit,
                KeyCode::Char('?') => {
                    app.mode = Mode::Help;
                    KeyAction::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    app.select_next();
                    KeyAction::None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    app.select_prev();
                    KeyAction::None
                }
                KeyCode::Tab | KeyCode::Char('l') | KeyCode::Enter => {
                    if !app.workers.is_empty() {
                        app.focus = Panel::Conversation;
                    }
                    KeyAction::None
                }
                KeyCode::Char('n') => {
                    app.ensure_prompts_loaded();
                    app.input_buffer.clear();
                    app.input_cursor = 0;
                    if app.repos.len() > 1 {
                        app.repo_select_index = 0;
                        app.mode = Mode::RepoSelect;
                    } else {
                        app.pending_repo =
                            app.repos.first().map(|r| crate::core::git::repo_name(r));
                        app.mode = Mode::CreatePrompt;
                    }
                    KeyAction::None
                }
                KeyCode::Char('x') => {
                    if let Some(w) = app.selected_worker() {
                        let id = w.id.clone();
                        app.confirm_message = format!("Close {}? (y/n)", id);
                        app.pending_action = Some(PendingAction::Close(id));
                        app.mode = Mode::Confirm;
                    }
                    KeyAction::None
                }
                KeyCode::Char('m') => {
                    if let Some(w) = app.selected_worker() {
                        let id = w.id.clone();
                        app.confirm_message = format!("Merge {}? (y/n)", id);
                        app.pending_action = Some(PendingAction::Merge(id));
                        app.mode = Mode::Confirm;
                    }
                    KeyAction::None
                }
                KeyCode::Char('s') => {
                    if app.selected_worker().is_some() {
                        app.input_buffer.clear();
                        app.input_cursor = 0;
                        app.mode = Mode::Input;
                    }
                    KeyAction::None
                }
                KeyCode::Char('p') => {
                    // PR detail always targets the parent worker
                    if let Some(w) = app.selected_parent()
                        && let Some(detail) = PrDetailInfo::from_worker(w)
                    {
                        app.pr_detail = Some(detail);
                        app.mode = Mode::PrDetail;
                    }
                    KeyAction::None
                }
                KeyCode::Char('z') => {
                    app.zoomed = !app.zoomed;
                    if app.zoomed && !app.workers.is_empty() {
                        app.focus = Panel::Conversation;
                    }
                    KeyAction::None
                }
                KeyCode::Char('G') | KeyCode::End => {
                    if let Some(conv) = app.selected_conversation_mut() {
                        conv.scroll_to_bottom();
                    }
                    KeyAction::None
                }
                _ => KeyAction::None,
            },
            Panel::Conversation => match key.code {
                KeyCode::Char('q') => KeyAction::Quit,
                KeyCode::Char('?') => {
                    app.mode = Mode::Help;
                    KeyAction::None
                }
                KeyCode::Char('h') => {
                    app.focus = Panel::Sidebar;
                    if let Some(conv) = app.selected_conversation_mut() {
                        conv.clear_focus();
                    }
                    KeyAction::None
                }
                KeyCode::Tab => {
                    app.focus = Panel::Sidebar;
                    if let Some(conv) = app.selected_conversation_mut() {
                        conv.clear_focus();
                    }
                    KeyAction::None
                }
                KeyCode::Char(']') => {
                    let vh = app.viewport_height;
                    if let Some(conv) = app.selected_conversation_mut() {
                        conv.focus_next_tool();
                        conv.scroll_to_focused(vh);
                    }
                    KeyAction::None
                }
                KeyCode::Char('[') => {
                    let vh = app.viewport_height;
                    if let Some(conv) = app.selected_conversation_mut() {
                        conv.focus_prev_tool();
                        conv.scroll_to_focused(vh);
                    }
                    KeyAction::None
                }
                KeyCode::Enter => {
                    if let Some(conv) = app.selected_conversation_mut() {
                        conv.toggle_focused_tool();
                    }
                    KeyAction::None
                }
                KeyCode::Esc => {
                    let has_focus = app
                        .selected_conversation()
                        .is_some_and(|c| c.focused_tool.is_some());
                    if has_focus {
                        if let Some(conv) = app.selected_conversation_mut() {
                            conv.clear_focus();
                        }
                    } else if app.zoomed {
                        app.zoomed = false;
                    } else {
                        app.focus = Panel::Sidebar;
                    }
                    KeyAction::None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if let Some(conv) = app.selected_conversation_mut() {
                        conv.scroll_up(1);
                    }
                    KeyAction::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if let Some(conv) = app.selected_conversation_mut() {
                        conv.scroll_down(1);
                    }
                    KeyAction::None
                }
                KeyCode::Char('u') => {
                    let amount = if key.modifiers.contains(KeyModifiers::CONTROL) {
                        (app.viewport_height / 2) as u32
                    } else {
                        1
                    };
                    if let Some(conv) = app.selected_conversation_mut() {
                        conv.scroll_up(amount);
                    }
                    KeyAction::None
                }
                KeyCode::Char('d') => {
                    let amount = if key.modifiers.contains(KeyModifiers::CONTROL) {
                        (app.viewport_height / 2) as u32
                    } else {
                        1
                    };
                    if let Some(conv) = app.selected_conversation_mut() {
                        conv.scroll_down(amount);
                    }
                    KeyAction::None
                }
                KeyCode::Char('G') | KeyCode::End => {
                    if let Some(conv) = app.selected_conversation_mut() {
                        conv.scroll_to_bottom();
                    }
                    KeyAction::None
                }
                KeyCode::Home => {
                    if let Some(conv) = app.selected_conversation_mut() {
                        let vh = if conv.conversation_area.height > 0 {
                            conv.conversation_area.height as u32
                        } else {
                            1
                        };
                        conv.scroll_offset = conv.total_visual_lines.saturating_sub(vh);
                        conv.auto_scroll = false;
                    }
                    KeyAction::None
                }
                KeyCode::PageUp => {
                    let vh = app.viewport_height as u32;
                    if let Some(conv) = app.selected_conversation_mut() {
                        conv.scroll_up(vh);
                    }
                    KeyAction::None
                }
                KeyCode::PageDown => {
                    let vh = app.viewport_height as u32;
                    if let Some(conv) = app.selected_conversation_mut() {
                        conv.scroll_down(vh);
                    }
                    KeyAction::None
                }
                KeyCode::Char('c') => {
                    if let Some(conv) = app.selected_conversation_mut() {
                        conv.toggle_all_tools();
                    }
                    KeyAction::None
                }
                KeyCode::Char('f') => {
                    if let Some(conv) = app.selected_conversation_mut() {
                        conv.filter_noise = !conv.filter_noise;
                        conv.focused_tool = None;
                    }
                    KeyAction::None
                }
                KeyCode::Char('i') | KeyCode::Char('s') => {
                    if app.selected_worker().is_some() {
                        app.input_buffer.clear();
                        app.input_cursor = 0;
                        app.mode = Mode::Input;
                    }
                    KeyAction::None
                }
                KeyCode::Char('p') => {
                    // PR detail always targets the parent worker
                    if let Some(w) = app.selected_parent()
                        && let Some(detail) = PrDetailInfo::from_worker(w)
                    {
                        app.pr_detail = Some(detail);
                        app.mode = Mode::PrDetail;
                    }
                    KeyAction::None
                }
                KeyCode::Char('z') => {
                    app.zoomed = !app.zoomed;
                    KeyAction::None
                }
                _ => KeyAction::None,
            },
        },
    }
}

/// Handle a mouse event and return the action to take.
fn handle_mouse(app: &mut DaemonTuiApp, mouse: crossterm::event::MouseEvent) -> KeyAction {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            if app.mode == Mode::Normal
                && app.focus == Panel::Conversation
                && let Some(conv) = app.selected_conversation_mut()
            {
                conv.scroll_up(5);
            }
        }
        MouseEventKind::ScrollDown => {
            if app.mode == Mode::Normal
                && app.focus == Panel::Conversation
                && let Some(conv) = app.selected_conversation_mut()
            {
                conv.scroll_down(5);
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if app.mode == Mode::Normal
                && app.focus == Panel::Conversation
                && let Some(conv) = app.selected_conversation_mut()
                && let Some(idx) = conv.entry_at_row(mouse.row)
            {
                conv.toggle_tool_at(idx);
            }
        }
        _ => {}
    }
    KeyAction::None
}

// ── Helpers ─────────────────────────────────────────────────

/// Fire-and-forget: send a request, don't wait for response.
/// Responses arrive asynchronously via the reader task channel.
/// On send failure, marks connection as disconnected for auto-reconnect.
async fn fire_and_forget(client: &mut DaemonClient, app: &mut DaemonTuiApp, req: DaemonRequest) {
    if let Err(e) = client.send(&req).await {
        handle_send_error(app, e);
    }
}

/// Handle a send error by marking disconnected and scheduling reconnect.
fn handle_send_error(app: &mut DaemonTuiApp, e: color_eyre::Report) {
    tui_log!(&app.work_dir, "send error -> disconnected: {}", e);
    app.connected = false;
    app.reconnect_at = Some(std::time::Instant::now());
    app.set_status(format!("disconnected: {}", e));
}

/// Kill and restart the daemon process. Used when the daemon is unresponsive
/// (accepting connections but not processing requests).
fn restart_daemon(work_dir: &std::path::Path) {
    // Kill existing daemon
    if let Some(pid) = crate::daemon::read_global_pid() {
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }

    // Start a fresh daemon in the background
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "swarm".to_string());

    let log_dir = work_dir.join(".swarm");
    std::fs::create_dir_all(&log_dir).ok();
    let daemon_log = std::fs::File::create(log_dir.join("daemon-stderr.log"))
        .unwrap_or_else(|_| std::fs::File::open("/dev/null").unwrap());

    let _ = std::process::Command::new(&exe)
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
        .spawn();
}

/// Attempt to reconnect to the daemon, spawning a new reader task.
async fn try_reconnect(
    app: &mut DaemonTuiApp,
    client: &mut DaemonClient,
    daemon_tx: &mpsc::UnboundedSender<Result<DaemonResponse>>,
    reader_task: &mut Option<tokio::task::JoinHandle<()>>,
) -> Result<()> {
    let new_client = tokio::time::timeout(
        Duration::from_millis(500),
        DaemonClient::connect(&app.work_dir),
    )
    .await
    .map_err(|_| color_eyre::eyre::eyre!("timeout"))??;
    *client = new_client;

    // Re-subscribe
    let sub_resp = tokio::time::timeout(Duration::from_millis(500), client.subscribe(None))
        .await
        .map_err(|_| color_eyre::eyre::eyre!("timeout"))??;
    match sub_resp {
        DaemonResponse::Ok { .. } => {}
        DaemonResponse::Error { message } => {
            return Err(color_eyre::eyre::eyre!("subscribe failed: {}", message));
        }
        _ => {}
    }

    // Fire-and-forget: ListWorkers response arrives through reader task
    client
        .send(&DaemonRequest::ListWorkers {
            workspace: Some(app.work_dir.clone()),
        })
        .await?;

    // Abort old reader task (prevents stale EOF from triggering reconnect)
    if let Some(task) = reader_task.take() {
        task.abort();
    }

    // Spawn new reader task on the same channel
    if let Some(reader) = client.take_reader() {
        *reader_task = Some(tokio::spawn(daemon_reader_task(reader, daemon_tx.clone())));
    }

    // Reset conversation state so history re-loads on select
    app.conversations.clear();
    app.history_loaded.clear();
    app.pending_history.clear();

    Ok(())
}

/// Handle a response or event from the daemon.
fn handle_daemon_response(app: &mut DaemonTuiApp, resp: DaemonResponse) {
    match resp {
        DaemonResponse::Ok { data } => {
            if let Some(ref d) = data {
                let keys: Vec<&String> = d
                    .as_object()
                    .map(|o| o.keys().collect())
                    .unwrap_or_default();
                tui_log!(&app.work_dir, "Ok response with data keys: {:?}", keys);

                // History response
                if let Some(content) = d.get("events").and_then(|v| v.as_str()) {
                    if let Some(wt_id) = app.pending_history.pop_front() {
                        let entries = app::parse_history_events(content);
                        tui_log!(
                            &app.work_dir,
                            "history for {}: {} bytes, {} entries parsed",
                            wt_id,
                            content.len(),
                            entries.len()
                        );
                        for entry in &entries {
                            match entry {
                                app::HistoryEntry::Event(event) => {
                                    app.handle_agent_event(&wt_id, event);
                                }
                                app::HistoryEntry::UserMessage(text) => {
                                    let conv = app
                                        .conversations
                                        .entry(wt_id.clone())
                                        .or_insert_with(app::WorkerConversation::new);
                                    conv.entries.push(
                                        crate::agent_tui::app::ConversationEntry::User {
                                            text: text.clone(),
                                            timestamp: chrono::Local::now()
                                                .format("%-I:%M %p")
                                                .to_string(),
                                        },
                                    );
                                }
                            }
                        }
                        // Flush any remaining streaming text into entries
                        // (history may end mid-stream without a TurnComplete).
                        if let Some(conv) = app.conversations.get_mut(&wt_id) {
                            conv.flush_streaming_text();
                            tui_log!(
                                &app.work_dir,
                                "history loaded for {}: {} conversation entries",
                                wt_id,
                                conv.entries.len()
                            );
                        }
                    } else {
                        tui_log!(
                            &app.work_dir,
                            "WARNING: history response but pending_history is empty!"
                        );
                    }
                    return;
                }
                // Create response
                if let Some(wt_id) = d.get("worktree_id").and_then(|v| v.as_str()) {
                    app.set_status(format!("created {}", wt_id));
                }
            }
        }
        DaemonResponse::Error { message } => {
            app.set_status(format!("error: {}", message));
        }
        DaemonResponse::Workers { workers } => {
            let ids: Vec<&str> = workers.iter().map(|w| w.id.as_str()).take(10).collect();
            tui_log!(
                &app.work_dir,
                "workers list: {} workers (first 10: {:?})",
                workers.len(),
                ids
            );
            app.update_worker_list(workers);
        }
        DaemonResponse::AgentEvent { worktree_id, event } => {
            tui_log!(
                &app.work_dir,
                "live event for {}: {:?}",
                worktree_id,
                std::mem::discriminant(&event)
            );
            app.handle_agent_event(&worktree_id, &event);
        }
        DaemonResponse::StateChanged { worktree_id, phase } => {
            app.handle_phase_change(&worktree_id, &phase);
        }
        DaemonResponse::Workspaces { .. } => {
            // Not used by the TUI directly
        }
        DaemonResponse::A2aTaskUpdate { .. } => {
            // A2A task updates are not consumed by the daemon TUI
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::state::WorkerPhase;
    use crate::daemon::protocol::{AgentEventWire, WorkerInfo};
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn make_worker(id: &str) -> WorkerInfo {
        WorkerInfo {
            id: id.into(),
            branch: format!("swarm/{}", id),
            prompt: "test".into(),
            agent: "claude".into(),
            phase: WorkerPhase::Running,
            session_id: None,
            pr_url: None,
            pr_number: None,
            pr_title: None,
            pr_state: None,
            restart_count: 0,
            created_at: None,
            agent_card: None,
            role: None,
            review_verdict: None,
        }
    }

    fn app_with_workers(ids: &[&str]) -> DaemonTuiApp {
        let mut app = DaemonTuiApp::new(std::path::PathBuf::from("/tmp/test"));
        app.update_worker_list(ids.iter().map(|id| make_worker(id)).collect());
        app
    }

    // ── handle_key: Normal mode ──

    #[test]
    fn key_x_enters_confirm_close() {
        let mut app = app_with_workers(&["w-1", "w-2"]);
        let action = handle_key(&mut app, key(KeyCode::Char('x')));
        assert!(matches!(action, KeyAction::None));
        assert!(matches!(app.mode, Mode::Confirm));
        assert!(matches!(app.pending_action, Some(PendingAction::Close(ref id)) if id == "w-1"));
    }

    #[test]
    fn key_m_enters_confirm_merge() {
        let mut app = app_with_workers(&["w-1"]);
        let action = handle_key(&mut app, key(KeyCode::Char('m')));
        assert!(matches!(action, KeyAction::None));
        assert!(matches!(app.mode, Mode::Confirm));
        assert!(matches!(app.pending_action, Some(PendingAction::Merge(ref id)) if id == "w-1"));
    }

    #[test]
    fn key_x_no_workers_does_nothing() {
        let mut app = DaemonTuiApp::new(std::path::PathBuf::from("/tmp"));
        let action = handle_key(&mut app, key(KeyCode::Char('x')));
        assert!(matches!(action, KeyAction::None));
        assert!(matches!(app.mode, Mode::Normal));
    }

    // ── handle_key: Confirm mode ──

    #[test]
    fn confirm_y_emits_close_action() {
        let mut app = app_with_workers(&["w-1"]);
        app.mode = Mode::Confirm;
        app.pending_action = Some(PendingAction::Close("w-1".into()));
        let action = handle_key(&mut app, key(KeyCode::Char('y')));
        assert!(matches!(action, KeyAction::CloseWorker(ref id) if id == "w-1"));
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn confirm_y_emits_merge_action() {
        let mut app = app_with_workers(&["w-1"]);
        app.mode = Mode::Confirm;
        app.pending_action = Some(PendingAction::Merge("w-1".into()));
        let action = handle_key(&mut app, key(KeyCode::Char('y')));
        assert!(matches!(action, KeyAction::MergeWorker(ref id) if id == "w-1"));
    }

    #[test]
    fn confirm_n_cancels() {
        let mut app = app_with_workers(&["w-1"]);
        app.mode = Mode::Confirm;
        app.pending_action = Some(PendingAction::Close("w-1".into()));
        let action = handle_key(&mut app, key(KeyCode::Char('n')));
        assert!(matches!(action, KeyAction::None));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.pending_action.is_none());
    }

    #[test]
    fn confirm_esc_cancels() {
        let mut app = app_with_workers(&["w-1"]);
        app.mode = Mode::Confirm;
        app.pending_action = Some(PendingAction::Close("w-1".into()));
        let action = handle_key(&mut app, key(KeyCode::Esc));
        assert!(matches!(action, KeyAction::None));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.pending_action.is_none());
    }

    // ── handle_key: Navigation ──

    #[test]
    fn key_j_k_navigation() {
        let mut app = app_with_workers(&["w-1", "w-2", "w-3"]);
        assert_eq!(app.selected, 0);
        handle_key(&mut app, key(KeyCode::Char('j')));
        assert_eq!(app.selected, 1);
        handle_key(&mut app, key(KeyCode::Char('j')));
        assert_eq!(app.selected, 2);
        handle_key(&mut app, key(KeyCode::Char('j')));
        assert_eq!(app.selected, 2); // clamped
        handle_key(&mut app, key(KeyCode::Char('k')));
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn key_q_emits_quit() {
        let mut app = app_with_workers(&["w-1"]);
        let action = handle_key(&mut app, key(KeyCode::Char('q')));
        assert!(matches!(action, KeyAction::Quit));
    }

    #[test]
    fn key_question_enters_help() {
        let mut app = app_with_workers(&["w-1"]);
        handle_key(&mut app, key(KeyCode::Char('?')));
        assert!(matches!(app.mode, Mode::Help));
    }

    #[test]
    fn help_any_key_dismisses() {
        let mut app = app_with_workers(&["w-1"]);
        app.mode = Mode::Help;
        handle_key(&mut app, key(KeyCode::Char('a')));
        assert!(matches!(app.mode, Mode::Normal));
    }

    // ── handle_key: Input mode ──

    #[test]
    fn input_enter_sends_message() {
        let mut app = app_with_workers(&["w-1"]);
        app.mode = Mode::Input;
        app.input_buffer = "hello".into();
        app.input_cursor = 5;
        let action = handle_key(&mut app, key(KeyCode::Enter));
        match action {
            KeyAction::SendMessage { worktree_id, text } => {
                assert_eq!(worktree_id, "w-1");
                assert_eq!(text, "hello");
            }
            _ => panic!("expected SendMessage, got {:?}", action),
        }
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn input_esc_cancels() {
        let mut app = app_with_workers(&["w-1"]);
        app.mode = Mode::Input;
        app.input_buffer = "draft".into();
        handle_key(&mut app, key(KeyCode::Esc));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.input_buffer.is_empty());
    }

    #[test]
    fn input_empty_enter_cancels() {
        let mut app = app_with_workers(&["w-1"]);
        app.mode = Mode::Input;
        app.input_buffer = "  ".into();
        let action = handle_key(&mut app, key(KeyCode::Enter));
        assert!(matches!(action, KeyAction::None));
    }

    // ── handle_daemon_response ──

    #[test]
    fn response_workers_updates_list() {
        let mut app = DaemonTuiApp::new(std::path::PathBuf::from("/tmp"));
        handle_daemon_response(
            &mut app,
            DaemonResponse::Workers {
                workers: vec![make_worker("w-1"), make_worker("w-2")],
            },
        );
        assert_eq!(app.workers.len(), 2);
        assert_eq!(app.workers[0].id, "w-1");
    }

    #[test]
    fn response_error_sets_status() {
        let mut app = DaemonTuiApp::new(std::path::PathBuf::from("/tmp"));
        handle_daemon_response(
            &mut app,
            DaemonResponse::Error {
                message: "not found".into(),
            },
        );
        assert!(app.status_message.as_ref().unwrap().0.contains("not found"));
    }

    #[test]
    fn response_ok_with_worktree_id_sets_status() {
        let mut app = DaemonTuiApp::new(std::path::PathBuf::from("/tmp"));
        handle_daemon_response(
            &mut app,
            DaemonResponse::Ok {
                data: Some(serde_json::json!({"worktree_id": "hive-3"})),
            },
        );
        assert!(app.status_message.as_ref().unwrap().0.contains("hive-3"));
    }

    #[test]
    fn response_agent_event_updates_conversation() {
        let mut app = app_with_workers(&["w-1"]);
        handle_daemon_response(
            &mut app,
            DaemonResponse::AgentEvent {
                worktree_id: "w-1".into(),
                event: AgentEventWire::TextDelta {
                    text: "hello".into(),
                },
            },
        );
        let conv = app.conversations.get("w-1").unwrap();
        assert_eq!(conv.streaming_text, "hello");
    }

    #[test]
    fn response_state_changed_updates_phase() {
        let mut app = app_with_workers(&["w-1"]);
        handle_daemon_response(
            &mut app,
            DaemonResponse::StateChanged {
                worktree_id: "w-1".into(),
                phase: WorkerPhase::Completed,
            },
        );
        assert_eq!(app.workers[0].phase, WorkerPhase::Completed);
    }

    // ── Full close flow (key -> action -> response) ──

    #[test]
    fn close_flow_x_then_y_produces_close_action() {
        let mut app = app_with_workers(&["w-1", "w-2"]);
        handle_key(&mut app, key(KeyCode::Char('j')));
        assert_eq!(app.selected, 1);

        handle_key(&mut app, key(KeyCode::Char('x')));
        assert!(matches!(app.mode, Mode::Confirm));
        assert!(matches!(app.pending_action, Some(PendingAction::Close(ref id)) if id == "w-2"));

        let action = handle_key(&mut app, key(KeyCode::Char('y')));
        assert!(matches!(action, KeyAction::CloseWorker(ref id) if id == "w-2"));
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn close_flow_refresh_removes_worker() {
        let mut app = app_with_workers(&["w-1", "w-2"]);
        handle_daemon_response(
            &mut app,
            DaemonResponse::Workers {
                workers: vec![make_worker("w-2")],
            },
        );
        assert_eq!(app.workers.len(), 1);
        assert_eq!(app.workers[0].id, "w-2");
    }

    // ── Panel focus ──

    #[test]
    fn tab_switches_to_conversation_panel() {
        let mut app = app_with_workers(&["w-1"]);
        assert!(matches!(app.focus, Panel::Sidebar));
        handle_key(&mut app, key(KeyCode::Tab));
        assert!(matches!(app.focus, Panel::Conversation));
    }

    #[test]
    fn h_switches_back_to_sidebar() {
        let mut app = app_with_workers(&["w-1"]);
        app.focus = Panel::Conversation;
        handle_key(&mut app, key(KeyCode::Char('h')));
        assert!(matches!(app.focus, Panel::Sidebar));
    }

    #[test]
    fn tab_from_conversation_switches_to_sidebar() {
        let mut app = app_with_workers(&["w-1"]);
        app.focus = Panel::Conversation;
        handle_key(&mut app, key(KeyCode::Tab));
        assert!(matches!(app.focus, Panel::Sidebar));
    }

    #[test]
    fn tab_no_workers_stays_on_sidebar() {
        let mut app = DaemonTuiApp::new(std::path::PathBuf::from("/tmp"));
        handle_key(&mut app, key(KeyCode::Tab));
        assert!(matches!(app.focus, Panel::Sidebar));
    }

    // ── ModifierSelect flow ──

    #[test]
    fn agent_select_enter_goes_to_modifier_select() {
        let mut app = DaemonTuiApp::new(std::path::PathBuf::from("/tmp"));
        app.pending_prompt = "task".into();
        app.mode = Mode::AgentSelect;
        handle_key(&mut app, key(KeyCode::Enter));
        assert!(matches!(app.mode, Mode::ModifierSelect));
    }

    #[test]
    fn agent_select_number_goes_to_modifier_select() {
        let mut app = DaemonTuiApp::new(std::path::PathBuf::from("/tmp"));
        app.pending_prompt = "task".into();
        app.mode = Mode::AgentSelect;
        handle_key(&mut app, key(KeyCode::Char('1')));
        assert!(matches!(app.mode, Mode::ModifierSelect));
        assert_eq!(app.agent_select_index, 0);
    }

    #[test]
    fn modifier_select_navigation() {
        let mut app = DaemonTuiApp::new(std::path::PathBuf::from("/tmp"));
        app.ensure_prompts_loaded();
        app.mode = Mode::ModifierSelect;
        assert_eq!(app.modifier_cursor, 0);
        handle_key(&mut app, key(KeyCode::Char('j')));
        assert_eq!(app.modifier_cursor, 1);
        handle_key(&mut app, key(KeyCode::Char('k')));
        assert_eq!(app.modifier_cursor, 0);
    }

    #[test]
    fn modifier_select_toggle() {
        let mut app = DaemonTuiApp::new(std::path::PathBuf::from("/tmp"));
        app.ensure_prompts_loaded();
        app.mode = Mode::ModifierSelect;
        assert!(!app.modifier_selected[0]);
        handle_key(&mut app, key(KeyCode::Char(' ')));
        assert!(app.modifier_selected[0]);
        handle_key(&mut app, key(KeyCode::Char(' ')));
        assert!(!app.modifier_selected[0]);
    }

    #[test]
    fn modifier_select_toggle_all() {
        let mut app = DaemonTuiApp::new(std::path::PathBuf::from("/tmp"));
        app.ensure_prompts_loaded();
        app.mode = Mode::ModifierSelect;
        handle_key(&mut app, key(KeyCode::Char('a')));
        assert!(app.modifier_selected.iter().all(|&s| s));
        handle_key(&mut app, key(KeyCode::Char('a')));
        assert!(app.modifier_selected.iter().all(|&s| !s));
    }

    #[test]
    fn modifier_select_enter_creates_worker() {
        let mut app = DaemonTuiApp::new(std::path::PathBuf::from("/tmp"));
        app.pending_prompt = "task".into();
        app.mode = Mode::ModifierSelect;
        let action = handle_key(&mut app, key(KeyCode::Enter));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(matches!(action, KeyAction::CreateWorker { .. }));
    }

    #[test]
    fn modifier_select_esc_goes_to_agent_select() {
        let mut app = DaemonTuiApp::new(std::path::PathBuf::from("/tmp"));
        app.mode = Mode::ModifierSelect;
        handle_key(&mut app, key(KeyCode::Esc));
        assert!(matches!(app.mode, Mode::AgentSelect));
    }
}
