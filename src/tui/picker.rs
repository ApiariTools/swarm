use crate::core::{agent::AgentKind, git, ipc};
use chrono::Local;
use color_eyre::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Paragraph};
use std::io::stdout;
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

use super::theme;

enum Phase {
    RepoSelect,
    Input,
    AgentSelect,
}

struct Picker {
    work_dir: PathBuf,
    repos: Vec<PathBuf>,
    phase: Phase,
    repo_index: usize,
    input_buffer: String,
    input_cursor: usize,
    agent_index: usize,
}

impl Picker {
    fn new(work_dir: PathBuf, repos: Vec<PathBuf>) -> Self {
        let phase = if repos.len() > 1 {
            Phase::RepoSelect
        } else {
            Phase::Input
        };
        Self {
            work_dir,
            repos,
            phase,
            repo_index: 0,
            input_buffer: String::new(),
            input_cursor: 0,
            agent_index: 0,
        }
    }
}

/// Run the popup picker TUI (repo → task → agent). Writes to IPC inbox on confirm.
pub fn run_picker(work_dir: PathBuf, repos: Vec<PathBuf>) -> Result<()> {
    stdout().execute(EnterAlternateScreen)?;
    enable_raw_mode()?;

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    terminal.clear()?;

    let mut picker = Picker::new(work_dir, repos);
    let result = picker_loop(&mut terminal, &mut picker);

    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;

    result
}

fn picker_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    picker: &mut Picker,
) -> Result<()> {
    loop {
        terminal.draw(|frame| draw_picker(frame, picker))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.code == KeyCode::Char('c')
                {
                    break;
                }

                match picker.phase {
                    Phase::RepoSelect => match key.code {
                        KeyCode::Esc => break,
                        KeyCode::Char('j') | KeyCode::Down => {
                            picker.repo_index =
                                (picker.repo_index + 1) % picker.repos.len();
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            picker.repo_index = if picker.repo_index == 0 {
                                picker.repos.len() - 1
                            } else {
                                picker.repo_index - 1
                            };
                        }
                        KeyCode::Enter => {
                            picker.phase = Phase::Input;
                        }
                        KeyCode::Char(c @ '1'..='9') => {
                            let idx = (c as usize) - ('1' as usize);
                            if idx < picker.repos.len() {
                                picker.repo_index = idx;
                                picker.phase = Phase::Input;
                            }
                        }
                        _ => {}
                    },
                    Phase::Input => match key.code {
                        KeyCode::Esc => {
                            if picker.repos.len() > 1 {
                                picker.phase = Phase::RepoSelect;
                                picker.input_buffer.clear();
                                picker.input_cursor = 0;
                            } else {
                                break;
                            }
                        }
                        KeyCode::Enter => {
                            if !picker.input_buffer.trim().is_empty() {
                                picker.phase = Phase::AgentSelect;
                            }
                        }
                        KeyCode::Backspace => {
                            if picker.input_cursor > 0 {
                                picker.input_cursor -= 1;
                                picker.input_buffer.remove(picker.input_cursor);
                            }
                        }
                        KeyCode::Char(c) => {
                            picker.input_buffer.insert(picker.input_cursor, c);
                            picker.input_cursor += 1;
                        }
                        KeyCode::Left => {
                            if picker.input_cursor > 0 {
                                picker.input_cursor -= 1;
                            }
                        }
                        KeyCode::Right => {
                            if picker.input_cursor < picker.input_buffer.len() {
                                picker.input_cursor += 1;
                            }
                        }
                        _ => {}
                    },
                    Phase::AgentSelect => match key.code {
                        KeyCode::Esc => {
                            picker.phase = Phase::Input;
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            picker.agent_index =
                                (picker.agent_index + 1) % AgentKind::all().len();
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            let count = AgentKind::all().len();
                            picker.agent_index = if picker.agent_index == 0 {
                                count - 1
                            } else {
                                picker.agent_index - 1
                            };
                        }
                        KeyCode::Enter => {
                            submit_picker(picker)?;
                            break;
                        }
                        KeyCode::Char('1') => {
                            picker.agent_index = 0;
                            submit_picker(picker)?;
                            break;
                        }
                        KeyCode::Char('2') => {
                            if AgentKind::all().len() > 1 {
                                picker.agent_index = 1;
                                submit_picker(picker)?;
                                break;
                            }
                        }
                        _ => {}
                    },
                }
            }
        }
    }

    Ok(())
}

fn submit_picker(picker: &Picker) -> Result<()> {
    let agents = AgentKind::all();
    let agent = &agents[picker.agent_index];
    let repo_name = if picker.repos.len() > 1 {
        Some(git::repo_name(&picker.repos[picker.repo_index]))
    } else {
        None
    };

    let msg = ipc::InboxMessage::Create {
        id: Uuid::new_v4().to_string(),
        prompt: picker.input_buffer.clone(),
        agent: agent.label().to_string(),
        repo: repo_name,
        timestamp: Local::now(),
    };
    ipc::write_inbox(&picker.work_dir, &msg)?;
    Ok(())
}

// ── Drawing ───────────────────────────────────────────────

fn draw_picker(frame: &mut Frame, picker: &Picker) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::COMB)),
        area,
    );

    match picker.phase {
        Phase::RepoSelect => draw_repo_phase(frame, area, picker),
        Phase::Input => draw_input_phase(frame, area, picker),
        Phase::AgentSelect => draw_agent_phase(frame, area, picker),
    }
}

fn draw_repo_phase(frame: &mut Frame, area: Rect, picker: &Picker) {
    let inner = area;

    // Title
    let title = Line::from(Span::styled(" select repo", theme::title()));
    frame.render_widget(
        Paragraph::new(title),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    for (i, repo) in picker.repos.iter().enumerate() {
        let is_selected = i == picker.repo_index;
        let y = inner.y + 2 + i as u16;
        if y >= inner.y + inner.height.saturating_sub(1) {
            break;
        }
        let name = git::repo_name(repo);

        let line = Line::from(vec![
            Span::styled(
                if is_selected { " \u{25b8} " } else { "   " },
                if is_selected { theme::selected() } else { theme::muted() },
            ),
            Span::styled(format!("{} ", i + 1), theme::muted()),
            Span::styled(
                name,
                if is_selected { theme::selected() } else { theme::text() },
            ),
        ]);
        frame.render_widget(Paragraph::new(line), Rect::new(inner.x, y, inner.width, 1));
    }

    // Hint at bottom
    let hint = Line::from(vec![
        Span::styled("j/k", theme::key_hint()),
        Span::styled(" navigate  ", theme::key_desc()),
        Span::styled("enter", theme::key_hint()),
        Span::styled(" select  ", theme::key_desc()),
        Span::styled("esc", theme::key_hint()),
        Span::styled(" cancel", theme::key_desc()),
    ]);
    let hint_y = inner.y + inner.height.saturating_sub(1);
    frame.render_widget(
        Paragraph::new(hint),
        Rect::new(inner.x + 1, hint_y, inner.width.saturating_sub(2), 1),
    );
}

fn draw_input_phase(frame: &mut Frame, area: Rect, picker: &Picker) {
    let inner = area;

    // Show repo context if multi-repo
    if picker.repos.len() > 1 {
        let repo_name = git::repo_name(&picker.repos[picker.repo_index]);
        let ctx = Line::from(vec![
            Span::styled(" repo: ", theme::muted()),
            Span::styled(repo_name, theme::accent()),
        ]);
        frame.render_widget(
            Paragraph::new(ctx),
            Rect::new(inner.x, inner.y + 1, inner.width, 1),
        );
    }

    // Input line centered vertically
    let input_y = inner.y + inner.height / 2;

    let before: String = picker.input_buffer.chars().take(picker.input_cursor).collect();
    let cursor_char = picker
        .input_buffer
        .chars()
        .nth(picker.input_cursor)
        .unwrap_or(' ');
    let after: String = picker
        .input_buffer
        .chars()
        .skip(picker.input_cursor + 1)
        .collect();

    let input_line = Line::from(vec![
        Span::styled(" > ", theme::accent()),
        Span::styled(before, theme::text()),
        Span::styled(
            cursor_char.to_string(),
            Style::default().fg(theme::COMB).bg(theme::HONEY),
        ),
        Span::styled(after, theme::text()),
    ]);
    frame.render_widget(
        Paragraph::new(input_line),
        Rect::new(inner.x, input_y, inner.width, 1),
    );

    // Hint at bottom
    let hint = Line::from(vec![
        Span::styled("enter", theme::key_hint()),
        Span::styled(" submit  ", theme::key_desc()),
        Span::styled("esc", theme::key_hint()),
        Span::styled(" back", theme::key_desc()),
    ]);
    let hint_y = inner.y + inner.height.saturating_sub(1);
    frame.render_widget(
        Paragraph::new(hint),
        Rect::new(inner.x + 1, hint_y, inner.width.saturating_sub(2), 1),
    );
}

fn draw_agent_phase(frame: &mut Frame, area: Rect, picker: &Picker) {
    let agents = AgentKind::all();
    let inner = area;

    // Title
    let title = Line::from(Span::styled(" select agent", theme::title()));
    frame.render_widget(
        Paragraph::new(title),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    for (i, agent) in agents.iter().enumerate() {
        let is_selected = i == picker.agent_index;
        let y = inner.y + 2 + i as u16;
        if y >= inner.y + inner.height.saturating_sub(3) {
            break;
        }

        let line = Line::from(vec![
            Span::styled(
                if is_selected { " \u{25b8} " } else { "   " },
                if is_selected { theme::selected() } else { theme::muted() },
            ),
            Span::styled(format!("{} ", i + 1), theme::muted()),
            Span::styled(
                agent.name(),
                if is_selected { theme::selected() } else { theme::text() },
            ),
        ]);
        frame.render_widget(Paragraph::new(line), Rect::new(inner.x, y, inner.width, 1));
    }

    // Show task context
    let task_line = Line::from(vec![
        Span::styled(" task: ", theme::muted()),
        Span::styled(&picker.input_buffer, theme::accent()),
    ]);
    let task_y = inner.y + inner.height.saturating_sub(3);
    frame.render_widget(
        Paragraph::new(task_line),
        Rect::new(inner.x, task_y, inner.width, 1),
    );

    // Hint
    let hint = Line::from(vec![
        Span::styled("j/k", theme::key_hint()),
        Span::styled(" navigate  ", theme::key_desc()),
        Span::styled("enter", theme::key_hint()),
        Span::styled(" confirm  ", theme::key_desc()),
        Span::styled("esc", theme::key_hint()),
        Span::styled(" back", theme::key_desc()),
    ]);
    let hint_y = inner.y + inner.height.saturating_sub(1);
    frame.render_widget(
        Paragraph::new(hint),
        Rect::new(inner.x + 1, hint_y, inner.width.saturating_sub(2), 1),
    );
}
