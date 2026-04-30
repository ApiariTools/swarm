pub mod app;
pub mod events;
pub mod markdown;
pub mod render;

use apiari_claude_sdk::streaming::AssembledEvent;
use apiari_claude_sdk::types::ContentBlock;
use apiari_claude_sdk::{ClaudeClient, Event, SessionOptions};
use app::{InputMode, SdkEvent, SessionStatus, TuiApp};
use color_eyre::Result;
use crossterm::ExecutableCommand;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, KeyCode, KeyModifiers, MouseButton,
    MouseEventKind,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use events::EventLogger;
use ratatui::prelude::*;
use std::io::stdout;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;

use crate::core::ipc;

/// Arguments for the agent-tui subcommand.
pub struct AgentTuiArgs {
    pub prompt: String,
    pub worktree_id: Option<String>,
    pub dangerously_skip_permissions: bool,
    pub work_dir: PathBuf,
}

/// Run the agent TUI.
pub async fn run(args: AgentTuiArgs) -> Result<()> {
    let (sdk_tx, sdk_rx) = mpsc::unbounded_channel::<SdkEvent>();
    let (followup_tx, followup_rx) = mpsc::unbounded_channel::<String>();

    // Set up event logger path
    let wt_id = args
        .worktree_id
        .clone()
        .unwrap_or_else(|| "default".to_string());
    let event_log_path = args
        .work_dir
        .join(".swarm")
        .join("agents")
        .join(&wt_id)
        .join("events.jsonl");
    let logger = EventLogger::new(event_log_path.clone());

    // Check for a previous session to restore
    let previous = events::read_last_session(&event_log_path);

    // Create app state
    let mut app = TuiApp::new(sdk_rx);

    let resume_session_id = if let Some(prev) = previous {
        // Restore previous session state
        app.entries = prev.entries;
        app.session_id = Some(prev.session_id.clone());
        app.turn_count = prev.turns;
        app.cost_usd = prev.cost_usd;
        app.tool_count = prev.tool_count;
        app.model = prev.model;
        app.status = SessionStatus::Waiting;
        app.entries.push(app::ConversationEntry::Status {
            text: "Restored previous session — press i to send a follow-up".to_string(),
        });
        // Write agent-status file immediately so the sidebar sees "waiting"
        if let Some(ref wt) = args.worktree_id {
            let status_dir = args.work_dir.join(".swarm").join("agent-status");
            let _ = std::fs::create_dir_all(&status_dir);
            let _ = std::fs::write(status_dir.join(wt), "waiting");
        }
        Some(prev.session_id)
    } else {
        // Fresh session — log start and add initial user prompt
        logger.log_start(&args.prompt, None);
        app.entries.push(app::ConversationEntry::User {
            text: args.prompt.clone(),
            timestamp: chrono::Local::now().format("%-I:%M %p").to_string(),
        });
        None
    };

    let is_restored = resume_session_id.is_some();

    // Spawn the SDK session in a background task
    let prompt = args.prompt.clone();
    let dangerously_skip = args.dangerously_skip_permissions;
    let work_dir = args.work_dir.clone();
    let bg_logger = EventLogger::new(event_log_path);

    tokio::spawn(async move {
        if let Err(e) = run_sdk_session(
            prompt,
            dangerously_skip,
            work_dir,
            sdk_tx.clone(),
            followup_rx,
            bg_logger,
            resume_session_id,
        )
        .await
        {
            let _ = sdk_tx.send(SdkEvent::Error(format!("SDK session error: {}", e)));
        }
    });

    // Run the TUI
    stdout().execute(EnterAlternateScreen)?;
    stdout().execute(EnableMouseCapture)?;
    enable_raw_mode()?;

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    terminal.clear()?;

    let result = event_loop(
        &mut terminal,
        &mut app,
        &followup_tx,
        &args.work_dir,
        args.worktree_id.as_deref(),
        is_restored,
    )
    .await;

    disable_raw_mode()?;
    stdout().execute(DisableMouseCapture)?;
    stdout().execute(LeaveAlternateScreen)?;

    result
}

/// The SDK session runner — sends prompt, drains events, then loops waiting for
/// follow-up messages and resuming the session.
///
/// If `resume_session_id` is `Some`, skip the initial spawn+prompt and jump
/// straight to the follow-up wait loop (restoring a previous session).
async fn run_sdk_session(
    prompt: String,
    dangerously_skip: bool,
    work_dir: PathBuf,
    tx: mpsc::UnboundedSender<SdkEvent>,
    mut followup_rx: mpsc::UnboundedReceiver<String>,
    logger: EventLogger,
    resume_session_id: Option<String>,
) -> Result<()> {
    let client = ClaudeClient::new();

    let mut current_session_id: Option<String> = resume_session_id.clone();

    // If we're restoring a previous session, skip the initial spawn+prompt
    if resume_session_id.is_none() {
        // Spawn initial session
        let opts = SessionOptions {
            dangerously_skip_permissions: dangerously_skip,
            include_partial_messages: true,
            working_dir: Some(work_dir.clone()),
            ..Default::default()
        };

        let mut session = match client.spawn(opts).await {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("Failed to start Claude: {}", e);
                logger.log_error(&msg);
                let _ = tx.send(SdkEvent::Error(msg));
                return Ok(());
            }
        };

        // Send the initial prompt
        if let Err(e) = session.send_message(&prompt).await {
            let msg = format!("Failed to send prompt: {}", e);
            logger.log_error(&msg);
            let _ = tx.send(SdkEvent::Error(msg));
            return Ok(());
        }

        // Drain events from the initial session until Result
        let got_result =
            drain_session_events(&mut session, &tx, &logger, &mut current_session_id).await;

        if !got_result && current_session_id.is_none() {
            return Ok(());
        }

        // Signal the TUI that we're now waiting for messages
        if let Some(ref sid) = current_session_id {
            let _ = tx.send(SdkEvent::SessionWaiting {
                session_id: sid.clone(),
            });
        }
    }

    // Follow-up wait loop — shared by both fresh and restored sessions
    loop {
        // Wait for a follow-up message (from user input or agent inbox)
        let message = match followup_rx.recv().await {
            Some(msg) => msg,
            None => break, // Channel closed — TUI quit
        };

        // Log the follow-up message
        logger.log_user_message(&message);

        // Resume the session with the captured session_id
        let resume_opts = SessionOptions {
            resume: current_session_id.clone(),
            dangerously_skip_permissions: dangerously_skip,
            include_partial_messages: true,
            working_dir: Some(work_dir.clone()),
            ..Default::default()
        };

        let mut session = match client.spawn(resume_opts).await {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("Failed to resume session: {}", e);
                logger.log_error(&msg);
                let _ = tx.send(SdkEvent::Error(msg));
                continue;
            }
        };

        // Send the follow-up message
        if let Err(e) = session.send_message(&message).await {
            let msg = format!("Failed to send message: {}", e);
            logger.log_error(&msg);
            let _ = tx.send(SdkEvent::Error(msg));
            continue;
        }

        // Drain events from the resumed session
        let got_result =
            drain_session_events(&mut session, &tx, &logger, &mut current_session_id).await;

        if !got_result && current_session_id.is_none() {
            break;
        }

        // Signal the TUI that we're now waiting for messages
        if let Some(ref sid) = current_session_id {
            let _ = tx.send(SdkEvent::SessionWaiting {
                session_id: sid.clone(),
            });
        }
    }

    Ok(())
}

/// Drain events from a session until a Result event or EOF.
/// Returns true if a Result event was received.
async fn drain_session_events(
    session: &mut apiari_claude_sdk::Session,
    tx: &mpsc::UnboundedSender<SdkEvent>,
    logger: &EventLogger,
    session_id: &mut Option<String>,
) -> bool {
    loop {
        match session.next_event().await {
            Ok(Some(event)) => {
                let is_result = event.is_result();
                // Capture session_id from Result
                if let Event::Result(ref result) = event {
                    *session_id = Some(result.session_id.clone());
                }
                process_sdk_event(&event, tx, logger);
                if is_result {
                    return true;
                }
            }
            Ok(None) => return false,
            Err(e) => {
                let msg = format!("SDK error: {}", e);
                logger.log_error(&msg);
                let _ = tx.send(SdkEvent::Error(msg));
                return false;
            }
        }
    }
}

/// Convert an SDK Event into TUI SdkEvents and forward them.
fn process_sdk_event(event: &Event, tx: &mpsc::UnboundedSender<SdkEvent>, logger: &EventLogger) {
    match event {
        Event::System(sys) => {
            let model = sys
                .data
                .get("model")
                .and_then(|v| v.as_str())
                .map(String::from);
            let _ = tx.send(SdkEvent::System { model });
        }
        Event::Stream { assembled, .. } => {
            for asm in assembled {
                match asm {
                    AssembledEvent::TextDelta { text, .. } => {
                        let _ = tx.send(SdkEvent::TextDelta(text.clone()));
                    }
                    AssembledEvent::ContentBlockComplete { block, .. } => {
                        match block {
                            ContentBlock::ToolUse { name, input, .. } => {
                                let input_str = serde_json::to_string(input)
                                    .unwrap_or_else(|_| input.to_string());
                                logger.log_tool_use(name, &input_str);
                            }
                            ContentBlock::ToolResult {
                                content, is_error, ..
                            } => {
                                let output = content
                                    .as_ref()
                                    .map(|v| {
                                        v.as_str()
                                            .map(String::from)
                                            .unwrap_or_else(|| v.to_string())
                                    })
                                    .unwrap_or_default();
                                logger.log_tool_result("", &output, is_error.unwrap_or(false));
                            }
                            ContentBlock::Text { text } => {
                                logger.log_text(text);
                            }
                            _ => {}
                        }
                        let _ = tx.send(SdkEvent::ContentBlock(block.clone()));
                    }
                    AssembledEvent::MessageComplete { .. } => {
                        let _ = tx.send(SdkEvent::TurnComplete);
                    }
                    AssembledEvent::ThinkingDelta { .. } => {
                        let _ = tx.send(SdkEvent::ThinkingDelta);
                    }
                    AssembledEvent::MessageStart { .. } => {}
                }
            }
        }
        Event::Assistant { message, .. } => {
            // In non-streaming mode, we get full assistant messages.
            for block in &message.message.content {
                match block {
                    ContentBlock::ToolUse { name, input, .. } => {
                        let input_str =
                            serde_json::to_string(input).unwrap_or_else(|_| input.to_string());
                        logger.log_tool_use(name, &input_str);
                    }
                    ContentBlock::Text { text } => {
                        logger.log_text(text);
                    }
                    _ => {}
                }
                let _ = tx.send(SdkEvent::ContentBlock(block.clone()));
            }
            let _ = tx.send(SdkEvent::TurnComplete);
        }
        Event::Result(result) => {
            logger.log_session_result(
                result.num_turns,
                result.total_cost_usd,
                Some(&result.session_id),
            );
            let _ = tx.send(SdkEvent::Result {
                turns: result.num_turns,
                cost_usd: result.total_cost_usd,
                session_id: result.session_id.clone(),
                is_error: result.is_error,
            });
        }
        Event::User(_) | Event::RateLimit(_) => {}
    }
}

/// The main TUI event loop.
async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut TuiApp,
    followup_tx: &mpsc::UnboundedSender<String>,
    work_dir: &Path,
    worktree_id: Option<&str>,
    is_restored: bool,
) -> Result<()> {
    // Track inbox offset for polling per-agent inbox.
    // On restore, skip to end of inbox to avoid re-processing old messages.
    let mut inbox_offset: u64 = if is_restored {
        if let Some(wt_id) = worktree_id {
            let inbox_path = work_dir
                .join(".swarm")
                .join("agents")
                .join(wt_id)
                .join("inbox.jsonl");
            std::fs::metadata(&inbox_path).map(|m| m.len()).unwrap_or(0)
        } else {
            0
        }
    } else {
        0
    };
    let mut inbox_poll_counter: u64 = 0;
    let mut prev_status = app.status.clone();

    loop {
        terminal.draw(|frame| render::draw(frame, app))?;

        // Drain SDK events and advance animation tick
        app.drain_sdk_events();
        app.validate_focus();
        app.tick();

        // Write agent status file when SessionStatus transitions to/from Waiting
        if app.status != prev_status {
            if let Some(wt_id) = worktree_id {
                let became_waiting = app.status == SessionStatus::Waiting;
                let was_waiting = prev_status == SessionStatus::Waiting;
                if became_waiting || was_waiting {
                    let status_str = if became_waiting { "waiting" } else { "running" };
                    let status_dir = work_dir.join(".swarm").join("agent-status");
                    let _ = std::fs::create_dir_all(&status_dir);
                    let _ = std::fs::write(status_dir.join(wt_id), status_str);
                }
            }
            prev_status = app.status.clone();
        }

        // Poll per-agent inbox every ~500ms (every 10 ticks at 50ms each)
        inbox_poll_counter += 1;
        if inbox_poll_counter.is_multiple_of(10)
            && let Some(wt_id) = worktree_id
            && app.status == SessionStatus::Waiting
            && let Ok((messages, new_offset)) = ipc::read_agent_inbox(work_dir, wt_id, inbox_offset)
        {
            inbox_offset = new_offset;
            for msg in messages {
                app.add_user_message(msg.message.clone());
                let _ = followup_tx.send(msg.message);
                app.auto_scroll = true;
            }
        }

        let poll_ms = 50;

        if event::poll(Duration::from_millis(poll_ms))? {
            match event::read()? {
                event::Event::Key(key) => {
                    // Ctrl+C always quits
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c')
                    {
                        break;
                    }

                    match app.input_mode {
                        InputMode::Normal => match key.code {
                            KeyCode::Char('q') => break,
                            KeyCode::Char('i')
                                if app.status == SessionStatus::Done
                                    || app.status == SessionStatus::Waiting =>
                            {
                                app.input_mode = InputMode::Input;
                            }
                            KeyCode::Tab => {
                                app.focus_next_tool();
                                app.scroll_to_focused();
                            }
                            KeyCode::BackTab => {
                                app.focus_prev_tool();
                                app.scroll_to_focused();
                            }
                            KeyCode::Enter if app.focused_tool.is_some() => {
                                app.toggle_focused_tool();
                                app.scroll_to_focused();
                            }
                            KeyCode::Esc => {
                                app.clear_focus();
                            }
                            KeyCode::PageUp | KeyCode::Char('u') => {
                                app.scroll_up(app.viewport_height as u32 / 2);
                            }
                            KeyCode::PageDown | KeyCode::Char('d') => {
                                app.scroll_down(app.viewport_height as u32 / 2);
                            }
                            KeyCode::Up | KeyCode::Char('k') => app.scroll_up(1),
                            KeyCode::Down | KeyCode::Char('j') => app.scroll_down(1),
                            KeyCode::Char('G') | KeyCode::End => app.scroll_to_bottom(),
                            KeyCode::Char('c') => app.toggle_all_tools(),
                            _ => {}
                        },
                        InputMode::Input => match key.code {
                            KeyCode::Esc => {
                                app.input_mode = InputMode::Normal;
                                app.input_buffer.clear();
                                app.input_cursor = 0;
                            }
                            KeyCode::Enter => {
                                let text = app.take_input();
                                if !text.trim().is_empty() {
                                    app.add_user_message(text.clone());
                                    let _ = followup_tx.send(text);
                                    app.auto_scroll = true;
                                }
                                app.input_mode = InputMode::Normal;
                            }
                            KeyCode::Backspace => app.input_backspace(),
                            KeyCode::Left => app.input_cursor_left(),
                            KeyCode::Right => app.input_cursor_right(),
                            KeyCode::Char(c) => app.input_char(c),
                            _ => {}
                        },
                    }
                }
                event::Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => app.scroll_up(3),
                    MouseEventKind::ScrollDown => app.scroll_down(3),
                    MouseEventKind::Down(MouseButton::Left) => {
                        if app.input_mode == InputMode::Normal
                            && let Some(idx) = app.entry_at_row(mouse.row)
                        {
                            app.toggle_tool_at(idx);
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }

    Ok(())
}
