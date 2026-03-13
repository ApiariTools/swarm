use crate::agent_tui::app::{ConversationEntry, InputMode, SessionStatus, TuiApp};
use crate::tui::theme;
use apiari_tui::conversation;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};
use std::time::Duration;

/// Draw the entire agent TUI.
pub fn draw(frame: &mut Frame, app: &mut TuiApp) {
    let area = frame.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title bar
            Constraint::Min(1),    // conversation area
            Constraint::Length(if app.input_mode == InputMode::Input {
                3
            } else {
                0
            }), // input area
            Constraint::Length(1), // status bar
        ])
        .split(area);

    // Use conversation area height minus bottom padding for scroll calculations
    app.viewport_height = chunks[1].height.saturating_sub(2);

    draw_title_bar(frame, chunks[0]);
    draw_conversation(frame, chunks[1], app);
    if app.input_mode == InputMode::Input {
        draw_input(frame, chunks[2], app);
    }
    draw_status_bar(frame, chunks[3], app);
}

fn draw_title_bar(frame: &mut Frame, area: Rect) {
    let title = Line::from(vec![Span::styled(
        "  Claude TUI Agent",
        Style::default()
            .fg(theme::FROST)
            .add_modifier(Modifier::BOLD),
    )]);
    frame.render_widget(Paragraph::new(title), area);
}

fn draw_conversation(frame: &mut Frame, area: Rect, app: &mut TuiApp) {
    let block = Block::default().padding(Padding::new(0, 0, 0, 2));
    let inner = block.inner(area);
    app.conversation_area = inner;
    let mut lines: Vec<Line<'_>> = Vec::new();

    // Use shared conversation renderer for entries
    let entry_line_map =
        conversation::render_conversation(&mut lines, &app.entries, app.focused_tool, None);

    // Streaming text (not yet flushed) — swarm-specific
    if !app.streaming_text.is_empty() {
        let need_header = !is_last_entry_assistant_or_tool(&app.entries);
        if need_header {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(
                    "  Claude:",
                    Style::default()
                        .fg(theme::FROST)
                        .add_modifier(Modifier::BOLD),
                ),
                if !app.streaming_timestamp.is_empty() {
                    Span::styled(
                        format!("  {}", app.streaming_timestamp),
                        Style::default().fg(theme::SMOKE),
                    )
                } else {
                    Span::raw("")
                },
            ]));
        }
        for line in app.streaming_text.lines() {
            lines.push(Line::from(Span::styled(
                format!("  {}", line),
                theme::text(),
            )));
        }
        if app.is_streaming {
            lines.push(Line::from(Span::styled(
                "  \u{258c}",
                Style::default()
                    .fg(theme::FROST)
                    .add_modifier(Modifier::BOLD),
            )));
        }
    } else if app.is_streaming {
        lines.push(Line::from(Span::styled(
            "  \u{258c}",
            Style::default()
                .fg(theme::FROST)
                .add_modifier(Modifier::BOLD),
        )));
    }

    app.entry_line_map = entry_line_map;
    app.total_rendered_lines = lines.len() as u32;

    // Calculate scroll using inner height (accounts for bottom padding)
    let visible_height = inner.height as u32;

    if app.auto_scroll {
        // Trim to a small tail so our visual-line estimate stays accurate
        // (drift accumulates over many lines with ratatui's word-wrapping).
        let keep_lines = (visible_height as usize) * 4 + 50;
        let w = inner.width.max(1) as usize;
        let display_lines = if lines.len() > keep_lines {
            &lines[lines.len() - keep_lines..]
        } else {
            &lines[..]
        };
        let mut tail_visual: u32 = 0;
        for line in display_lines {
            let lw = line.width();
            tail_visual += (lw.max(1).div_ceil(w)) as u32;
        }
        let scroll = tail_visual.saturating_sub(visible_height);
        let paragraph = Paragraph::new(Text::from(display_lines.to_vec()))
            .scroll((scroll.min(u16::MAX as u32) as u16, 0))
            .wrap(Wrap { trim: false })
            .block(block);
        frame.render_widget(paragraph, area);
    } else {
        let total_lines = lines.len() as u32;
        let target_scroll = total_lines
            .saturating_sub(visible_height)
            .saturating_sub(app.scroll_offset);

        let (display_lines, effective_scroll) = if target_scroll > 500 {
            let buffer = visible_height.max(100);
            let drop_target = target_scroll.saturating_sub(buffer);
            let mut drop_count = 0usize;
            let mut dropped = 0u32;
            let w = inner.width.max(1) as usize;
            for line in lines.iter() {
                let lw = line.width();
                let vl = (lw.max(1).div_ceil(w)) as u32;
                if dropped + vl > drop_target {
                    break;
                }
                dropped += vl;
                drop_count += 1;
            }
            let adj = target_scroll - dropped;
            (
                Text::from(lines[drop_count..].to_vec()),
                adj.min(u16::MAX as u32) as u16,
            )
        } else {
            (Text::from(lines), target_scroll as u16)
        };

        let paragraph = Paragraph::new(display_lines)
            .scroll((effective_scroll, 0))
            .wrap(Wrap { trim: false })
            .block(block);
        frame.render_widget(paragraph, area);
    }
}

/// Check if the last entry is an AssistantText or ToolCall (for streaming header merging).
fn is_last_entry_assistant_or_tool(entries: &[ConversationEntry]) -> bool {
    matches!(
        entries.last(),
        Some(ConversationEntry::AssistantText { .. } | ConversationEntry::ToolCall { .. })
    )
}

fn draw_input(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border_active())
        .title(Span::styled(" Follow-up ", theme::accent()))
        .padding(Padding::horizontal(1));

    let input_text = Paragraph::new(app.input_buffer.as_str())
        .style(theme::text())
        .block(input_block);

    frame.render_widget(input_text, area);

    // Place cursor
    frame.set_cursor_position((area.x + 2 + app.input_cursor as u16, area.y + 1));
}

fn draw_status_bar(frame: &mut Frame, area: Rect, app: &TuiApp) {
    const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let spin = SPINNER[(app.tick_count / 4) as usize % SPINNER.len()];

    let elapsed = app.last_event_at.elapsed();
    let stalled = elapsed > Duration::from_secs(30);

    let (status_text, status_style) = match &app.status {
        SessionStatus::Starting => {
            let s = if stalled {
                format!("{} starting ({}s)", spin, elapsed.as_secs())
            } else {
                format!("{} starting...", spin)
            };
            (s, theme::status_running())
        }
        SessionStatus::Thinking => {
            let s = if stalled {
                format!("{} thinking ({}s)", spin, elapsed.as_secs())
            } else {
                format!("{} thinking...", spin)
            };
            (s, theme::status_running())
        }
        SessionStatus::Streaming => {
            let s = if stalled {
                format!("{} streaming ({}s)", spin, elapsed.as_secs())
            } else {
                format!("{} streaming...", spin)
            };
            (s, theme::status_running())
        }
        SessionStatus::ToolRunning => {
            let s = if stalled {
                format!("{} running tool ({}s)", spin, elapsed.as_secs())
            } else {
                format!("{} running tool...", spin)
            };
            (s, theme::status_running())
        }

        SessionStatus::Waiting => {
            let dot = if (app.tick_count / 8).is_multiple_of(2) {
                "○"
            } else {
                "●"
            };
            (format!("{} waiting...", dot), theme::status_idle())
        }
        SessionStatus::Done => {
            let s = if let Some(cost) = app.cost_usd {
                format!("● done (${:.4})", cost)
            } else {
                "● done".to_string()
            };
            (s, theme::status_done())
        }
        SessionStatus::Errored => ("✖ error".to_string(), theme::error()),
    };

    let model_str = app.model.as_deref().unwrap_or("unknown");

    let right_info = format!(
        "tools: {} | turns: {} | {}",
        app.tool_count, app.turn_count, model_str
    );

    let scroll_hint = if !app.auto_scroll {
        format!("↑{} ", app.scroll_offset)
    } else {
        String::new()
    };

    let hint = if app.focused_tool.is_some() {
        format!(" {}tab:next s-tab:prev enter:toggle esc:done ", scroll_hint)
    } else if app.status == SessionStatus::Done || app.status == SessionStatus::Waiting {
        format!(" {}tab:tool u/d:page c:tools i:input q:quit ", scroll_hint)
    } else {
        format!(" {}tab:tool u/d:page c:tools q:quit ", scroll_hint)
    };

    // Build the status line
    let available = area.width as usize;
    let left = format!(" [{}]", status_text);
    let right = format!("{}  {} ", hint, right_info);
    let padding = available.saturating_sub(left.len() + right.len());

    let line = Line::from(vec![
        Span::styled(left, status_style),
        Span::styled(" ".repeat(padding), theme::muted()),
        Span::styled(right, theme::muted()),
    ]);

    let bar = Paragraph::new(line).style(Style::default().bg(theme::COMB));
    frame.render_widget(bar, area);
}
