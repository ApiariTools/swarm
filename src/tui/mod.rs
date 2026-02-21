pub mod app;
pub mod picker;
pub mod render;
pub mod theme;

use app::App;
use color_eyre::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::prelude::*;
use std::io::stdout;
use std::time::Duration;

/// Run the TUI event loop.
pub async fn run(app: &mut App) -> Result<()> {
    stdout().execute(EnterAlternateScreen)?;
    enable_raw_mode()?;

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    terminal.clear()?;

    let result = event_loop(&mut terminal, app).await;

    // Cleanup
    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;

    result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    loop {
        terminal.draw(|frame| render::draw(frame, app))?;

        let poll_ms = 100;

        if event::poll(Duration::from_millis(poll_ms))? {
            if let Event::Key(key) = event::read()? {
                // Ctrl+C always quits
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.code == KeyCode::Char('c')
                {
                    app.save_state();
                    break;
                }

                match app.mode {
                    app::Mode::Normal => match key.code {
                        KeyCode::Char('q') => {
                            app.save_state();
                            break;
                        }
                        KeyCode::Char('n') => app.start_new_worktree(),
                        KeyCode::Char('t') => app.add_terminal_to_selected(),
                        KeyCode::Char('j') | KeyCode::Down => app.select_next(),
                        KeyCode::Char('k') | KeyCode::Up => app.select_prev(),
                        KeyCode::Enter => app.jump_to_selected(),
                        KeyCode::Char('m') => app.start_merge_selected(),
                        KeyCode::Char('x') => app.start_close_selected(),
                        KeyCode::Char('?') => app.toggle_help(),
                        _ => {}
                    },
                    app::Mode::Input => match key.code {
                        KeyCode::Esc => app.cancel_input(),
                        KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
                            app.input_char('\n');
                        }
                        KeyCode::Enter => app.submit_input().await,
                        KeyCode::Backspace => app.input_backspace(),
                        KeyCode::Char(c) => app.input_char(c),
                        KeyCode::Left => app.input_cursor_left(),
                        KeyCode::Right => app.input_cursor_right(),
                        _ => {}
                    },
                    app::Mode::RepoSelect => match key.code {
                        KeyCode::Esc => app.cancel_input(),
                        KeyCode::Char('j') | KeyCode::Down => app.repo_select_next(),
                        KeyCode::Char('k') | KeyCode::Up => app.repo_select_prev(),
                        KeyCode::Enter => app.confirm_repo(),
                        KeyCode::Char(c @ '1'..='9') => {
                            app.select_repo_by_index((c as usize) - ('1' as usize)).await;
                        }
                        KeyCode::Char(c @ 'a'..='z') if c != 'j' && c != 'k' => {
                            app.select_repo_by_index(9 + (c as usize) - ('a' as usize)).await;
                        }
                        _ => {}
                    },
                    app::Mode::AgentSelect => match key.code {
                        KeyCode::Esc => app.cancel_input(),
                        KeyCode::Char('j') | KeyCode::Down => app.agent_select_next(),
                        KeyCode::Char('k') | KeyCode::Up => app.agent_select_prev(),
                        KeyCode::Enter => app.confirm_agent().await,
                        KeyCode::Char('1') => app.select_agent_by_index(0).await,
                        KeyCode::Char('2') => app.select_agent_by_index(1).await,
                        KeyCode::Char('3') => app.select_agent_by_index(2).await,
                        _ => {}
                    },
                    app::Mode::Confirm => match key.code {
                        KeyCode::Char('y') | KeyCode::Enter => app.confirm_action().await,
                        KeyCode::Char('n') | KeyCode::Esc => app.cancel_input(),
                        _ => {}
                    },
                    app::Mode::Help => match key.code {
                        KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
                            app.toggle_help()
                        }
                        _ => {}
                    },
                }
            }
        }

        // Refresh states periodically
        app.tick();
    }

    Ok(())
}
