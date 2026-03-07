use crate::agent_tui::app::{ConversationEntry, InputMode, SessionStatus, TuiApp};
use crate::agent_tui::markdown;
use crate::tui::theme;
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
    let mut entry_line_map: Vec<(u32, u32)> = Vec::with_capacity(app.entries.len());

    let w = inner.width as usize;
    let mut last_shown_ts = String::new();
    for (i, entry) in app.entries.iter().enumerate() {
        let start = lines.len() as u32;
        let is_focused = app.focused_tool == Some(i);
        match entry {
            ConversationEntry::User { text, timestamp } => {
                // Divider before user messages (visual turn boundary)
                if i > 0 {
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::styled(
                        format!("  {}", "─".repeat(w.saturating_sub(4))),
                        Style::default().fg(theme::STEEL),
                    )));
                }
                lines.push(Line::from(""));
                let ts_span = dedup_timestamp(timestamp, &mut last_shown_ts);
                lines.push(Line::from(vec![
                    Span::styled(
                        "  You:",
                        Style::default()
                            .fg(theme::HONEY)
                            .add_modifier(Modifier::BOLD),
                    ),
                    ts_span,
                ]));
                for line in text.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("  {}", line),
                        theme::text(),
                    )));
                }
            }
            ConversationEntry::AssistantText { text, timestamp } => {
                // Merge headers: skip if previous entry (looking past tool calls) was assistant
                let in_same_turn = is_continuation_of_assistant_turn(&app.entries, i);
                if !in_same_turn {
                    lines.push(Line::from(""));
                    let ts_span = dedup_timestamp(timestamp, &mut last_shown_ts);
                    lines.push(Line::from(vec![
                        Span::styled(
                            "  Claude:",
                            Style::default()
                                .fg(theme::FROST)
                                .add_modifier(Modifier::BOLD),
                        ),
                        ts_span,
                    ]));
                }
                lines.extend(markdown::render_markdown(text));
            }
            ConversationEntry::ToolCall {
                tool,
                input,
                output,
                is_error,
                collapsed,
            } => {
                let focus_prefix = if is_focused { "▶ " } else { "  " };
                let tool_style_expanded = if is_focused {
                    Style::default()
                        .fg(theme::HONEY)
                        .add_modifier(Modifier::BOLD)
                } else {
                    theme::tool_name()
                };
                if *collapsed {
                    // Collapsed: single-line summary — dimmed to stay in background
                    let (icon, icon_style) = if output.is_none() {
                        ("⋯", theme::muted())
                    } else if *is_error {
                        ("✖", theme::error())
                    } else {
                        ("✔", Style::default().fg(theme::STEEL))
                    };
                    let collapsed_tool_style = if is_focused {
                        Style::default()
                            .fg(theme::HONEY)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme::STEEL)
                    };
                    let preview = input
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(50)
                        .collect::<String>();
                    let ellipsis = if input.lines().next().is_some_and(|l| l.len() > 50) {
                        "..."
                    } else {
                        ""
                    };
                    lines.push(Line::from(vec![
                        Span::styled(format!("{}{} ", focus_prefix, icon), icon_style),
                        Span::styled(tool.as_str(), collapsed_tool_style),
                        Span::styled(
                            format!("  {}{}", preview, ellipsis),
                            Style::default().fg(theme::STEEL),
                        ),
                    ]));
                } else {
                    // Expanded: full tool block
                    lines.push(Line::from(""));
                    // Tool header
                    lines.push(Line::from(vec![
                        Span::styled(focus_prefix, theme::muted()),
                        Span::styled(format!(" {} ", tool), tool_style_expanded),
                        Span::styled(
                            " ────────────────────────────",
                            Style::default().fg(theme::STEEL),
                        ),
                    ]));
                    // Input
                    for line in input.lines().take(5) {
                        lines.push(Line::from(Span::styled(
                            format!("  │ {}", line),
                            Style::default().fg(theme::SLATE),
                        )));
                    }
                    if input.lines().count() > 5 {
                        lines.push(Line::from(Span::styled(
                            format!("  │ ... ({} more lines)", input.lines().count() - 5),
                            theme::muted(),
                        )));
                    }
                    // Output
                    if let Some(out) = output {
                        let out_style = if *is_error {
                            theme::error()
                        } else {
                            theme::muted()
                        };
                        lines.push(Line::from(Span::styled(
                            "  ├──────────────────────────────",
                            Style::default().fg(theme::STEEL),
                        )));
                        for line in out.lines().take(10) {
                            lines
                                .push(Line::from(Span::styled(format!("  │ {}", line), out_style)));
                        }
                        if out.lines().count() > 10 {
                            lines.push(Line::from(Span::styled(
                                format!("  │ ... ({} more lines)", out.lines().count() - 10),
                                theme::muted(),
                            )));
                        }
                    }
                    lines.push(Line::from(Span::styled(
                        "  └──────────────────────────────",
                        Style::default().fg(theme::STEEL),
                    )));
                }
            }
            ConversationEntry::Status { text } => {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    format!("  {}", text),
                    theme::muted(),
                )));
            }
        }
        let count = lines.len() as u32 - start;
        entry_line_map.push((start, count));
    }

    // Streaming text (not yet flushed)
    if !app.streaming_text.is_empty() {
        // Show Claude header unless this is a continuation of the same turn
        let need_header = !is_continuation_of_assistant_turn(&app.entries, app.entries.len());
        if need_header {
            lines.push(Line::from(""));
            let ts_span = dedup_timestamp(&app.streaming_timestamp, &mut last_shown_ts);
            lines.push(Line::from(vec![
                Span::styled(
                    "  Claude:",
                    Style::default()
                        .fg(theme::FROST)
                        .add_modifier(Modifier::BOLD),
                ),
                ts_span,
            ]));
        }
        for line in app.streaming_text.lines() {
            lines.push(Line::from(Span::styled(
                format!("  {}", line),
                theme::text(),
            )));
        }
        // Streaming cursor
        if app.is_streaming {
            lines.push(Line::from(Span::styled(
                "  \u{258c}",
                Style::default()
                    .fg(theme::FROST)
                    .add_modifier(Modifier::BOLD),
            )));
        }
    } else if app.is_streaming {
        // Just the cursor when streaming hasn't produced text yet
        lines.push(Line::from(Span::styled(
            "  \u{258c}",
            Style::default()
                .fg(theme::FROST)
                .add_modifier(Modifier::BOLD),
        )));
    }

    app.entry_line_map = entry_line_map;
    app.total_rendered_lines = lines.len() as u32;

    let text = Text::from(lines);
    let total_lines = text.lines.len() as u32;

    // Calculate scroll using inner height (accounts for bottom padding)
    let visible_height = inner.height as u32;
    let scroll = if app.auto_scroll {
        total_lines.saturating_sub(visible_height)
    } else {
        total_lines
            .saturating_sub(visible_height)
            .saturating_sub(app.scroll_offset)
    };

    let paragraph = Paragraph::new(text)
        .scroll((scroll as u16, 0))
        .wrap(Wrap { trim: false })
        .block(block);

    frame.render_widget(paragraph, area);
}

/// Check if entry at `idx` is a continuation of a Claude turn (looking past tool calls).
/// Returns true if there's an AssistantText before this one with only ToolCalls in between.
fn is_continuation_of_assistant_turn(entries: &[ConversationEntry], idx: usize) -> bool {
    if idx == 0 {
        return false;
    }
    // Walk backwards past tool calls
    for j in (0..idx).rev() {
        match &entries[j] {
            ConversationEntry::AssistantText { .. } => return true,
            ConversationEntry::ToolCall { .. } => continue,
            _ => return false,
        }
    }
    false
}

/// Show timestamp only if it differs from the last shown one. Updates `last_shown`.
fn dedup_timestamp<'a>(timestamp: &'a str, last_shown: &mut String) -> Span<'a> {
    if timestamp == last_shown.as_str() || timestamp.is_empty() {
        Span::raw("")
    } else {
        *last_shown = timestamp.to_string();
        Span::styled(
            format!("  {}", timestamp),
            Style::default().fg(theme::SMOKE),
        )
    }
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
