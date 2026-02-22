pub mod app;
pub mod events;
pub mod render;

use app::{InputMode, SdkEvent, SessionStatus, TuiApp};
use apiari_claude_sdk::streaming::AssembledEvent;
use apiari_claude_sdk::types::ContentBlock;
use apiari_claude_sdk::{ClaudeClient, Event, SessionOptions};
use color_eyre::Result;
use crossterm::event::{self, KeyCode, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::ExecutableCommand;
use events::EventLogger;
use ratatui::prelude::*;
use std::io::stdout;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

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
    let event_log_path = if let Some(ref wt_id) = args.worktree_id {
        args.work_dir
            .join(".swarm")
            .join("agents")
            .join(wt_id)
            .join("events.jsonl")
    } else {
        args.work_dir
            .join(".swarm")
            .join("agents")
            .join("default")
            .join("events.jsonl")
    };
    let logger = EventLogger::new(event_log_path.clone());

    // Log start
    logger.log_start(&args.prompt, None);

    // Spawn the SDK session in a background task
    let prompt = args.prompt.clone();
    let dangerously_skip = args.dangerously_skip_permissions;
    let work_dir = args.work_dir.clone();
    let bg_logger = EventLogger::new(event_log_path);

    tokio::spawn(async move {
        if let Err(e) =
            run_sdk_session(prompt, dangerously_skip, work_dir, sdk_tx, followup_rx, bg_logger)
                .await
        {
            eprintln!("SDK session error: {}", e);
        }
    });

    // Create app state
    let mut app = TuiApp::new(sdk_rx);

    // Add the initial user prompt as the first entry
    app.entries.push(app::ConversationEntry::User {
        text: args.prompt.clone(),
    });

    // Run the TUI
    stdout().execute(EnterAlternateScreen)?;
    enable_raw_mode()?;

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    terminal.clear()?;

    let result = event_loop(&mut terminal, &mut app, &followup_tx).await;

    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;

    // Log completion
    logger.log_complete(
        app.turn_count as u64,
        app.cost_usd,
        app.session_id.as_deref(),
    );

    result
}

/// The SDK session runner — sends prompt, drains events, accepts follow-ups.
async fn run_sdk_session(
    prompt: String,
    dangerously_skip: bool,
    work_dir: PathBuf,
    tx: mpsc::UnboundedSender<SdkEvent>,
    mut followup_rx: mpsc::UnboundedReceiver<String>,
    logger: EventLogger,
) -> Result<()> {
    let client = ClaudeClient::new();
    let opts = SessionOptions {
        dangerously_skip_permissions: dangerously_skip,
        include_partial_messages: true,
        working_dir: Some(work_dir),
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

    // Event draining loop — also checks for follow-up messages
    loop {
        tokio::select! {
            event_result = session.next_event() => {
                match event_result {
                    Ok(Some(event)) => {
                        let is_result = event.is_result();
                        process_sdk_event(&event, &tx, &logger);
                        if is_result {
                            // Session complete — wait for follow-ups
                            wait_for_followups(&mut session, &mut followup_rx, &tx, &logger).await;
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        let msg = format!("SDK error: {}", e);
                        logger.log_error(&msg);
                        let _ = tx.send(SdkEvent::Error(msg));
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

/// After the first turn completes, wait for follow-up messages and continue the conversation.
async fn wait_for_followups(
    _session: &mut apiari_claude_sdk::Session,
    followup_rx: &mut mpsc::UnboundedReceiver<String>,
    _tx: &mpsc::UnboundedSender<SdkEvent>,
    _logger: &EventLogger,
) {
    // If the session is finished (result received), we can't send more messages
    // to *this* session. The Claude CLI result means the conversation ended.
    // Multi-turn is handled within a single session before result is emitted.
    // So we just drain any pending follow-ups and ignore them.
    //
    // For true multi-turn, the session stays alive (no result yet) and we'd
    // need to handle this differently. For now, this is a placeholder.
    while let Ok(_msg) = followup_rx.try_recv() {
        // Session already finished, can't send follow-ups
    }
}

/// Convert an SDK Event into TUI SdkEvents and forward them.
fn process_sdk_event(
    event: &Event,
    tx: &mpsc::UnboundedSender<SdkEvent>,
    logger: &EventLogger,
) {
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
                                logger.log_tool_result(
                                    "",
                                    &output,
                                    is_error.unwrap_or(false),
                                );
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
                        let input_str = serde_json::to_string(input)
                            .unwrap_or_else(|_| input.to_string());
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
            logger.log_complete(
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
) -> Result<()> {
    loop {
        terminal.draw(|frame| render::draw(frame, app))?;

        // Drain SDK events and advance animation tick
        app.drain_sdk_events();
        app.tick();

        let poll_ms = 50;

        if event::poll(Duration::from_millis(poll_ms))? {
            if let event::Event::Key(key) = event::read()? {
                // Ctrl+C always quits
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.code == KeyCode::Char('c')
                {
                    break;
                }

                match app.input_mode {
                    InputMode::Normal => match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Char('i') => {
                            if app.status == SessionStatus::Done
                                || app.status == SessionStatus::Idle
                            {
                                app.input_mode = InputMode::Input;
                            }
                        }
                        KeyCode::PageUp | KeyCode::Char('u') => {
                            app.scroll_up(app.viewport_height.saturating_sub(2));
                        }
                        KeyCode::PageDown | KeyCode::Char('d') => {
                            app.scroll_down(app.viewport_height.saturating_sub(2));
                        }
                        KeyCode::Up | KeyCode::Char('k') => app.scroll_up(3),
                        KeyCode::Down | KeyCode::Char('j') => app.scroll_down(3),
                        KeyCode::Char('G') | KeyCode::End => app.scroll_to_bottom(),
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
        }
    }

    Ok(())
}
