use crate::core::{agent::AgentKind, git, ipc};
use chrono::Local;
use color_eyre::Result;
use crossterm::{
    ExecutableCommand,
    event::{self, Event, KeyCode, KeyModifiers},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Paragraph, Wrap};
use std::io::stdout;
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

use super::theme;

enum Phase {
    RepoSelect,
    Input,
    AgentSelect,
    Fetching,
    FetchConfirm,
}

struct Picker {
    work_dir: PathBuf,
    repos: Vec<PathBuf>,
    phase: Phase,
    repo_index: usize,
    input_buffer: String,
    input_cursor: usize,
    agent_index: usize,
    start_point: Option<String>,
    fetch_status: Option<String>,
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
            start_point: None,
            fetch_status: None,
        }
    }

    fn selected_repo(&self) -> PathBuf {
        self.repos
            .get(self.repo_index)
            .cloned()
            .unwrap_or_else(|| self.work_dir.clone())
    }
}

/// Run the popup picker TUI (repo -> task -> agent -> fetch). Writes to IPC inbox on confirm.
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

        // Fetching phase runs logic immediately (no key input needed)
        if matches!(picker.phase, Phase::Fetching) {
            if run_fetch(picker)? {
                break;
            }
            continue;
        }

        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                break;
            }

            match picker.phase {
                Phase::RepoSelect => match key.code {
                    KeyCode::Esc => break,
                    KeyCode::Char('j') | KeyCode::Down => {
                        picker.repo_index = (picker.repo_index + 1) % picker.repos.len();
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
                    KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
                        picker.input_buffer.insert(picker.input_cursor, '\n');
                        picker.input_cursor += 1;
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
                        picker.agent_index = (picker.agent_index + 1) % AgentKind::all().len();
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
                        picker.phase = Phase::Fetching;
                    }
                    KeyCode::Char('1') => {
                        picker.agent_index = 0;
                        picker.phase = Phase::Fetching;
                    }
                    KeyCode::Char('2') => {
                        if AgentKind::all().len() > 1 {
                            picker.agent_index = 1;
                            picker.phase = Phase::Fetching;
                        }
                    }
                    _ => {}
                },
                Phase::FetchConfirm => match key.code {
                    KeyCode::Char('y') | KeyCode::Enter => {
                        picker.start_point = Some("origin/main".to_string());
                        submit_picker(picker)?;
                        break;
                    }
                    KeyCode::Char('n') | KeyCode::Esc => {
                        picker.start_point = None;
                        submit_picker(picker)?;
                        break;
                    }
                    _ => {}
                },
                Phase::Fetching => unreachable!(),
            }
        }
    }

    Ok(())
}

/// Run the fetch + ff-only merge logic. Transitions to FetchConfirm or submits directly.
/// Returns Ok(true) if submitted (caller should break), Ok(false) if moved to FetchConfirm.
fn run_fetch(picker: &mut Picker) -> Result<bool> {
    let repo_path = picker.selected_repo();

    // Determine the local branch name
    let local_branch = match git::current_branch(&repo_path) {
        Ok(b) if b != "HEAD" => b,
        _ => {
            // Detached HEAD or error — skip fetch, submit directly
            picker.start_point = None;
            submit_picker(picker)?;
            return Ok(true);
        }
    };

    let remote_ref = format!("origin/{}", local_branch);

    // Try to fetch
    picker.fetch_status = Some("fetching origin...".to_string());
    let fetch_ok = git::fetch_origin(&repo_path).unwrap_or(false);

    if !fetch_ok {
        // No remote or offline — proceed with local HEAD
        picker.start_point = None;
        submit_picker(picker)?;
        return Ok(true);
    }

    // Check if local is behind
    let behind = git::commits_behind(&repo_path, &local_branch, &remote_ref).unwrap_or(0);
    if behind == 0 {
        // Already up to date
        picker.start_point = None;
        submit_picker(picker)?;
        return Ok(true);
    }

    // Local is behind — try ff-only merge
    let ff_ok = git::merge_ff_only(&repo_path, &remote_ref).unwrap_or(false);
    if ff_ok {
        // Successfully fast-forwarded, proceed with local HEAD (now current)
        picker.start_point = None;
        submit_picker(picker)?;
        return Ok(true);
    }

    // ff-only failed — local has diverged, ask user
    picker.fetch_status = Some(format!(
        "local {} has diverged from {}",
        local_branch, remote_ref
    ));
    picker.phase = Phase::FetchConfirm;
    Ok(false)
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
        start_point: picker.start_point.clone(),
        timestamp: Local::now(),
    };
    // Picker runs inside a tokio runtime (spawned from main), so use block_on
    tokio::runtime::Handle::current().block_on(ipc::send_inbox(&picker.work_dir, &msg))?;
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
        Phase::Fetching => draw_fetching_phase(frame, area, picker),
        Phase::FetchConfirm => draw_fetch_confirm_phase(frame, area, picker),
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
                if is_selected {
                    theme::selected()
                } else {
                    theme::muted()
                },
            ),
            Span::styled(format!("{} ", i + 1), theme::muted()),
            Span::styled(
                name,
                if is_selected {
                    theme::selected()
                } else {
                    theme::text()
                },
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

    // Track how much vertical space is used at the top
    let mut content_y = inner.y + 1;

    // Show repo context if multi-repo
    if picker.repos.len() > 1 {
        let repo_name = git::repo_name(&picker.repos[picker.repo_index]);
        let ctx = Line::from(vec![
            Span::styled(" repo: ", theme::muted()),
            Span::styled(repo_name, theme::accent()),
        ]);
        frame.render_widget(
            Paragraph::new(ctx),
            Rect::new(inner.x, content_y, inner.width, 1),
        );
        content_y += 2; // repo line + spacer
    }

    // Split buffer into lines and locate cursor
    let buf_lines: Vec<&str> = picker.input_buffer.split('\n').collect();
    let mut cursor_line = 0usize;
    let mut cursor_col = 0usize;
    let mut pos = 0usize;
    for (i, line) in buf_lines.iter().enumerate() {
        let line_chars = line.chars().count();
        if pos + line_chars >= picker.input_cursor && i < buf_lines.len() {
            cursor_line = i;
            cursor_col = picker.input_cursor - pos;
            break;
        }
        pos += line_chars + 1; // +1 for the \n
    }

    // Build styled lines with cursor highlight
    let mut styled_lines: Vec<Line> = Vec::new();
    for (i, line_str) in buf_lines.iter().enumerate() {
        let prefix = if i == 0 { " > " } else { "   " };
        let prefix_style = if i == 0 {
            theme::accent()
        } else {
            theme::text()
        };

        if i == cursor_line {
            let before: String = line_str.chars().take(cursor_col).collect();
            let cursor_char = line_str.chars().nth(cursor_col).unwrap_or(' ');
            let after: String = line_str.chars().skip(cursor_col + 1).collect();

            styled_lines.push(Line::from(vec![
                Span::styled(prefix, prefix_style),
                Span::styled(before, theme::text()),
                Span::styled(
                    cursor_char.to_string(),
                    Style::default().fg(theme::COMB).bg(theme::HONEY),
                ),
                Span::styled(after, theme::text()),
            ]));
        } else {
            styled_lines.push(Line::from(vec![
                Span::styled(prefix, prefix_style),
                Span::styled(line_str.to_string(), theme::text()),
            ]));
        }
    }

    // Reserve 1 row for the hint line at the bottom
    let input_height = (inner.y + inner.height).saturating_sub(content_y + 1);
    let input_area = Rect::new(inner.x, content_y, inner.width, input_height);

    // Calculate scroll to keep cursor visible
    let wrap_width = inner.width as usize;
    let mut visual_lines_before_cursor = 0usize;
    for (_i, line_str) in buf_lines.iter().enumerate().take(cursor_line) {
        let prefix_len = 3; // " > " or "   "
        let line_width = prefix_len + line_str.chars().count();
        visual_lines_before_cursor += 1 + line_width.saturating_sub(1) / wrap_width.max(1);
    }
    let cursor_prefix_len = 3;
    let cursor_line_width = cursor_prefix_len + cursor_col;
    visual_lines_before_cursor += cursor_line_width / wrap_width.max(1);

    let visible = input_height as usize;
    let scroll = if visual_lines_before_cursor >= visible {
        (visual_lines_before_cursor - visible + 1) as u16
    } else {
        0
    };

    let text = Text::from(styled_lines);
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        input_area,
    );

    // Hint at bottom
    let hint = Line::from(vec![
        Span::styled("\u{21b5}", theme::key_hint()),
        Span::styled(" submit  ", theme::key_desc()),
        Span::styled("alt+\u{21b5}", theme::key_hint()),
        Span::styled(" newline  ", theme::key_desc()),
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
                if is_selected {
                    theme::selected()
                } else {
                    theme::muted()
                },
            ),
            Span::styled(format!("{} ", i + 1), theme::muted()),
            Span::styled(
                agent.name(),
                if is_selected {
                    theme::selected()
                } else {
                    theme::text()
                },
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

fn draw_fetching_phase(frame: &mut Frame, area: Rect, picker: &Picker) {
    let inner = area;

    let status = picker
        .fetch_status
        .as_deref()
        .unwrap_or("fetching origin...");

    let line = Line::from(Span::styled(format!(" {}", status), theme::accent()));
    let y = inner.y + inner.height / 2;
    frame.render_widget(Paragraph::new(line), Rect::new(inner.x, y, inner.width, 1));
}

fn draw_fetch_confirm_phase(frame: &mut Frame, area: Rect, picker: &Picker) {
    let inner = area;

    let status = picker
        .fetch_status
        .as_deref()
        .unwrap_or("local branch has diverged from origin");

    // Status message
    let status_line = Line::from(Span::styled(format!(" {}", status), theme::text()));
    let y = inner.y + inner.height / 2 - 1;
    frame.render_widget(
        Paragraph::new(status_line),
        Rect::new(inner.x, y, inner.width, 1),
    );

    // Prompt
    let prompt_line = Line::from(Span::styled(
        " branch from origin instead? (y/n)",
        theme::accent(),
    ));
    frame.render_widget(
        Paragraph::new(prompt_line),
        Rect::new(inner.x, y + 2, inner.width, 1),
    );

    // Hint
    let hint = Line::from(vec![
        Span::styled("y", theme::key_hint()),
        Span::styled(" use origin  ", theme::key_desc()),
        Span::styled("n", theme::key_hint()),
        Span::styled(" use local", theme::key_desc()),
    ]);
    let hint_y = inner.y + inner.height.saturating_sub(1);
    frame.render_widget(
        Paragraph::new(hint),
        Rect::new(inner.x + 1, hint_y, inner.width.saturating_sub(2), 1),
    );
}
