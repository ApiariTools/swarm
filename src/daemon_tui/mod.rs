pub mod app;
pub mod render;
pub mod socket_client;

use app::{DaemonTuiApp, Mode, Panel, PendingAction, PrDetailInfo, daemon_agents};
use color_eyre::Result;
use crossterm::event::{self, KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use socket_client::DaemonClient;
use std::io::stdout;
use std::path::PathBuf;
use std::time::Duration;

use crate::core::review::{ReviewConfig, load_reviews_toml};
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

    // Fire-and-forget: request worker list. Response arrives in the event drain loop.
    if let Err(e) = client
        .send(&DaemonRequest::ListWorkers { workspace: None })
        .await
    {
        app.set_status(format!("list workers failed: {}", e));
    }

    // Remote mode: no repo detection
    // app.repos stays empty

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
    // Connect + subscribe with retries — on cold start the daemon may not be
    // fully ready even though Ping succeeded (socket server is up before the
    // main event loop processes Subscribe).
    let mut client = None;
    for attempt in 0..10 {
        match DaemonClient::connect(&work_dir).await {
            Ok(mut c) => match c.subscribe(None).await {
                Ok(DaemonResponse::Ok { .. }) => {
                    client = Some(c);
                    break;
                }
                Ok(DaemonResponse::Error { message }) => {
                    if attempt == 9 {
                        return Err(color_eyre::eyre::eyre!("subscribe failed: {}", message));
                    }
                }
                Ok(_) => {
                    client = Some(c);
                    break;
                }
                Err(_) => {}
            },
            Err(_) => {}
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut client = client.ok_or_else(|| {
        color_eyre::eyre::eyre!("failed to connect to daemon after 10 attempts")
    })?;

    let mut app = DaemonTuiApp::new(work_dir.clone());
    app.connected = true;

    // Fire-and-forget: request worker list. Response arrives in the event drain loop.
    // Don't use request_skipping_events — it blocks reading through all buffered events.
    if let Err(e) = client
        .send(&DaemonRequest::ListWorkers {
            workspace: Some(work_dir.clone()),
        })
        .await
    {
        app.set_status(format!("list workers failed: {}", e));
    }

    // Detect repos in background (spawns git subprocesses, can take seconds)
    let bg_work_dir = work_dir.clone();
    let repo_task = tokio::task::spawn_blocking(move || {
        crate::core::git::detect_repos(&bg_work_dir).unwrap_or_default()
    });

    // Enter terminal raw mode
    stdout().execute(EnterAlternateScreen)?;
    stdout().execute(crossterm::event::EnableMouseCapture)?;
    enable_raw_mode()?;

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    terminal.clear()?;

    let result = event_loop(&mut terminal, &mut app, &mut client, Some(repo_task)).await;

    disable_raw_mode()?;
    stdout().execute(crossterm::event::DisableMouseCapture)?;
    stdout().execute(LeaveAlternateScreen)?;

    result
}

/// The main TUI event loop.
async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut DaemonTuiApp,
    client: &mut DaemonClient,
    mut repo_task: Option<tokio::task::JoinHandle<Vec<PathBuf>>>,
) -> Result<()> {
    loop {
        if app.needs_redraw {
            terminal.draw(|frame| render::draw(frame, app))?;
            app.needs_redraw = false;
        }
        app.tick();
        if let Some(conv) = app.selected_conversation_mut() {
            conv.validate_focus();
        }

        // Check if background repo detection finished
        if let Some(ref task) = repo_task {
            if task.is_finished() {
                if let Some(task) = repo_task.take() {
                    if let Ok(repos) = task.await {
                        app.repos = repos;
                        app.needs_redraw = true;
                    }
                }
            }
        }

        // Lazy-load history for the selected worker on first view (fire-and-forget)
        if let Some(w) = app.selected_worker() {
            let wt_id = w.id.clone();
            if app.connected && !app.history_loaded.contains(&wt_id) {
                app.history_loaded.insert(wt_id.clone());
                app.pending_history.push_back(wt_id.clone());
                if let Err(e) = client
                    .send(&DaemonRequest::GetHistory {
                        worktree_id: wt_id,
                    })
                    .await
                {
                    app.pending_history.pop_back();
                    handle_send_error(app, e);
                }
            }
        }

        // Poll for crossterm events (keyboard + mouse) with short timeout
        let poll_ms = 100;
        if event::poll(Duration::from_millis(poll_ms))? {
            app.needs_redraw = true;
            let action = match event::read()? {
                event::Event::Key(key) => {
                    // Ctrl+C always quits
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c')
                    {
                        break;
                    }
                    handle_key(app, key)
                }
                event::Event::Mouse(mouse) => handle_mouse(app, mouse),
                _ => KeyAction::None,
            };

            match action {
                KeyAction::None => {}
                KeyAction::Quit => break,
                KeyAction::CreateWorker {
                    prompt,
                    agent,
                    repo,
                    review_configs,
                } => {
                    app.set_status("creating worker...".to_string());
                    let req = DaemonRequest::CreateWorker {
                        prompt,
                        agent,
                        repo,
                        start_point: None,
                        review_configs,
                        workspace: Some(app.work_dir.clone()),
                    };
                    fire_and_forget(client, app, req).await;
                    // Queue a list refresh — daemon processes FIFO so this
                    // runs after CreateWorker completes on the server side.
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
                            });
                        conv.auto_scroll = true;
                    }
                    fire_and_forget(client, app, req).await;
                }
                KeyAction::CloseWorker(id) => {
                    app.set_status(format!("closing {}...", id));
                    let req = DaemonRequest::CloseWorker {
                        worktree_id: id,
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
                KeyAction::MergeWorker(id) => {
                    app.set_status(format!("merging {}...", id));
                    let req = DaemonRequest::MergeWorker {
                        worktree_id: id,
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
            }
        }

        // Drain all buffered daemon responses (up to 100 per tick)
        if app.connected {
            for _ in 0..100 {
                match tokio::time::timeout(Duration::from_millis(1), client.next_response()).await {
                    Ok(Ok(resp)) => {
                        handle_daemon_response(app, resp);
                    }
                    Ok(Err(e)) => {
                        app.connected = false;
                        app.reconnect_at = Some(std::time::Instant::now());
                        app.set_status(format!("disconnected: {}", e));
                        break;
                    }
                    Err(_) => break, // no more buffered data
                }
            }
        } else if let Some(at) = app.reconnect_at {
            // Attempt reconnection every 500ms
            if at.elapsed() >= Duration::from_millis(500) {
                app.reconnect_at = Some(std::time::Instant::now());
                match try_reconnect(app, client).await {
                    Ok(()) => {
                        app.connected = true;
                        app.reconnect_at = None;
                        app.set_status("reconnected".to_string());
                    }
                    Err(_) => {
                        // Still disconnected, will retry
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
        review_configs: Option<Vec<ReviewConfig>>,
    },
    SendMessage {
        worktree_id: String,
        text: String,
    },
    CloseWorker(String),
    MergeWorker(String),
}

/// Handle a key event and return the action to take.
fn handle_key(app: &mut DaemonTuiApp, key: event::KeyEvent) -> KeyAction {
    match app.mode {
        Mode::Help => {
            // Any key dismisses help
            app.mode = Mode::Normal;
            KeyAction::None
        }
        Mode::PrDetail => match key.code {
            KeyCode::Char('o') | KeyCode::Enter => {
                if let Some(ref pr) = app.pr_detail {
                    let _ = std::process::Command::new("open")
                        .arg(&pr.url)
                        .spawn();
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
                app.pending_repo = app.repos.get(app.repo_select_index).map(|r| {
                    crate::core::git::repo_name(r)
                });
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
                app.review_cursor = 0;
                app.review_selected = vec![false; app.review_prompts.len()];
                app.mode = Mode::ReviewSelect;
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
                    app.review_cursor = 0;
                    app.review_selected = vec![false; app.review_prompts.len()];
                    app.mode = Mode::ReviewSelect;
                }
                KeyAction::None
            }
            _ => KeyAction::None,
        },
        Mode::ReviewSelect => match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                app.review_cursor = app.review_cursor.saturating_sub(1);
                KeyAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if app.review_cursor + 1 < app.review_prompts.len() {
                    app.review_cursor += 1;
                }
                KeyAction::None
            }
            KeyCode::Char(' ') => {
                if let Some(sel) = app.review_selected.get_mut(app.review_cursor) {
                    *sel = !*sel;
                }
                KeyAction::None
            }
            KeyCode::Char('a') => {
                let any_selected = app.review_selected.iter().any(|&s| s);
                let new_val = !any_selected;
                for sel in &mut app.review_selected {
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
                let prompt = std::mem::take(&mut app.pending_prompt);
                let repo = app.pending_repo.take();

                // Build review_configs from selections (same logic as picker.rs)
                let project_config = load_reviews_toml(&app.work_dir);
                let selected: Vec<ReviewConfig> = app
                    .review_prompts
                    .iter()
                    .zip(app.review_selected.iter())
                    .filter(|(_, sel)| **sel)
                    .map(|(rp, _)| {
                        let slug = rp.slug().to_string();
                        let entry = project_config
                            .as_ref()
                            .and_then(|p| p.reviews.get(&slug));
                        ReviewConfig {
                            prompt: rp.clone(),
                            agent: entry
                                .and_then(|e| e.agent.as_ref())
                                .and_then(|a| crate::core::agent::AgentKind::from_str(a)),
                            extra_instructions: entry.and_then(|e| e.prompt.clone()),
                            slug: Some(slug),
                            mode: entry.and_then(|e| e.mode).unwrap_or_default(),
                        }
                    })
                    .collect();

                app.mode = Mode::Normal;
                KeyAction::CreateWorker {
                    prompt,
                    agent,
                    repo,
                    review_configs: Some(selected),
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
        Mode::Normal => {
            match app.focus {
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
                        app.input_buffer.clear();
                        app.input_cursor = 0;
                        if app.repos.len() > 1 {
                            app.repo_select_index = 0;
                            app.mode = Mode::RepoSelect;
                        } else {
                            app.pending_repo = app.repos.first().map(|r| {
                                crate::core::git::repo_name(r)
                            });
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
                        // Quick send from sidebar
                        if app.selected_worker().is_some() {
                            app.input_buffer.clear();
                            app.input_cursor = 0;
                            app.mode = Mode::Input;
                        }
                        KeyAction::None
                    }
                    KeyCode::Char('p') => {
                        if let Some(w) = app.selected_worker()
                            && let Some(detail) = PrDetailInfo::from_worker(w)
                        {
                            app.pr_detail = Some(detail);
                            app.mode = Mode::PrDetail;
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
                        let vh = app.viewport_height;
                        if let Some(conv) = app.selected_conversation_mut() {
                            conv.focus_next_tool();
                            conv.scroll_to_focused(vh);
                        }
                        KeyAction::None
                    }
                    KeyCode::BackTab => {
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
                    KeyCode::Char('u')
                        if key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        let half = app.viewport_height / 2;
                        if let Some(conv) = app.selected_conversation_mut() {
                            conv.scroll_up(half as u32);
                        }
                        KeyAction::None
                    }
                    KeyCode::Char('d')
                        if key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        let half = app.viewport_height / 2;
                        if let Some(conv) = app.selected_conversation_mut() {
                            conv.scroll_down(half as u32);
                        }
                        KeyAction::None
                    }
                    KeyCode::Char('G') | KeyCode::End => {
                        if let Some(conv) = app.selected_conversation_mut() {
                            conv.scroll_to_bottom();
                        }
                        KeyAction::None
                    }
                    KeyCode::Char('c') => {
                        if let Some(conv) = app.selected_conversation_mut() {
                            conv.toggle_all_tools();
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
                        if let Some(w) = app.selected_worker()
                            && let Some(detail) = PrDetailInfo::from_worker(w)
                        {
                            app.pr_detail = Some(detail);
                            app.mode = Mode::PrDetail;
                        }
                        KeyAction::None
                    }
                    _ => KeyAction::None,
                },
            }
        }
    }
}

/// Handle a mouse event and return the action to take.
fn handle_mouse(app: &mut DaemonTuiApp, mouse: event::MouseEvent) -> KeyAction {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            if app.mode == Mode::Normal
                && app.focus == Panel::Conversation
                && let Some(conv) = app.selected_conversation_mut()
            {
                conv.scroll_up(3);
            }
        }
        MouseEventKind::ScrollDown => {
            if app.mode == Mode::Normal
                && app.focus == Panel::Conversation
                && let Some(conv) = app.selected_conversation_mut()
            {
                conv.scroll_down(3);
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

/// Fire-and-forget: send a request, don't wait for response.
/// Responses arrive asynchronously in the event drain loop.
/// On send failure, marks connection as disconnected for auto-reconnect.
async fn fire_and_forget(
    client: &mut DaemonClient,
    app: &mut DaemonTuiApp,
    req: DaemonRequest,
) {
    if let Err(e) = client.send(&req).await {
        handle_send_error(app, e);
    }
}

/// Handle a send error by marking disconnected and scheduling reconnect.
fn handle_send_error(app: &mut DaemonTuiApp, e: color_eyre::Report) {
    app.connected = false;
    app.reconnect_at = Some(std::time::Instant::now());
    app.set_status(format!("disconnected: {}", e));
}

/// Attempt to reconnect to the daemon, replaying state.
async fn try_reconnect(app: &mut DaemonTuiApp, client: &mut DaemonClient) -> Result<()> {
    // Use a short timeout so we don't block the event loop
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

    // Refresh worker list
    client
        .send(&DaemonRequest::ListWorkers {
            workspace: Some(app.work_dir.clone()),
        })
        .await?;
    let list_resp = tokio::time::timeout(Duration::from_millis(500), client.next_response())
        .await
        .map_err(|_| color_eyre::eyre::eyre!("timeout"))??;

    if let DaemonResponse::Workers { workers } = list_resp {
        app.conversations.clear();
        app.history_loaded.clear();
        app.pending_history.clear();
        app.update_worker_list(workers);
        // History will lazy-load on next worker select via the event loop
    }

    Ok(())
}

/// Handle a response or event from the daemon.
/// Process a daemon response and update app state accordingly.
fn handle_daemon_response(app: &mut DaemonTuiApp, resp: DaemonResponse) {
    match resp {
        DaemonResponse::Ok { data } => {
            if let Some(ref d) = data {
                // History response — apply events to the correct worker
                if let Some(content) = d.get("events").and_then(|v| v.as_str()) {
                    if let Some(wt_id) = app.pending_history.pop_front() {
                        let events = app::parse_history_events(content);
                        for event in &events {
                            app.handle_agent_event(&wt_id, event);
                        }
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
            app.update_worker_list(workers);
        }
        DaemonResponse::AgentEvent { worktree_id, event } => {
            app.handle_agent_event(&worktree_id, &event);
        }
        DaemonResponse::StateChanged {
            worktree_id,
            phase,
        } => {
            app.handle_phase_change(&worktree_id, &phase);
        }
        DaemonResponse::Workspaces { .. } => {
            // Not used by the TUI directly
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
            review_slugs: vec![],
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

    // ── Full close flow (key → action → response) ──

    #[test]
    fn close_flow_x_then_y_produces_close_action() {
        let mut app = app_with_workers(&["w-1", "w-2"]);
        // Select second worker
        handle_key(&mut app, key(KeyCode::Char('j')));
        assert_eq!(app.selected, 1);

        // Press x → confirm mode
        handle_key(&mut app, key(KeyCode::Char('x')));
        assert!(matches!(app.mode, Mode::Confirm));
        assert!(matches!(app.pending_action, Some(PendingAction::Close(ref id)) if id == "w-2"));

        // Press y → close action
        let action = handle_key(&mut app, key(KeyCode::Char('y')));
        assert!(matches!(action, KeyAction::CloseWorker(ref id) if id == "w-2"));
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn close_flow_refresh_removes_worker() {
        let mut app = app_with_workers(&["w-1", "w-2"]);
        // Simulate: close succeeded, daemon sends updated worker list without w-1
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
    fn tab_no_workers_stays_on_sidebar() {
        let mut app = DaemonTuiApp::new(std::path::PathBuf::from("/tmp"));
        handle_key(&mut app, key(KeyCode::Tab));
        assert!(matches!(app.focus, Panel::Sidebar));
    }
}
