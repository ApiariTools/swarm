use super::app::{DaemonTuiApp, Mode, Panel, WorkerConversation, daemon_agents, is_noise_tool};
use crate::agent_tui::app::ConversationEntry;
use crate::agent_tui::markdown;
use crate::core::state::WorkerPhase;
use crate::daemon::protocol::WorkerInfo;
use crate::tui::theme;

use chrono::Local;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};

/// Bright sidebar colors for the left bar indicator.
const SIDEBAR_COLORS: &[Color] = &[
    Color::Rgb(180, 120, 60), // warm brown
    Color::Rgb(60, 120, 180), // cool blue
    Color::Rgb(60, 180, 60),  // forest green
    Color::Rgb(140, 60, 180), // purple
    Color::Rgb(60, 180, 180), // teal
    Color::Rgb(180, 150, 60), // amber
    Color::Rgb(180, 60, 120), // rose
    Color::Rgb(100, 180, 60), // olive
];

const SPINNER: &[char] = &[
    '\u{2807}', '\u{280b}', '\u{2819}', '\u{2838}', '\u{2834}', '\u{2826}', '\u{2827}', '\u{280f}',
];

const SIDEBAR_WIDTH: u16 = 38;

/// Draw the entire daemon TUI.
pub fn draw(frame: &mut Frame, app: &mut DaemonTuiApp) {
    let area = frame.area();

    // Background
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::COMB)),
        area,
    );

    if app.zoomed {
        // Zoomed: conversation panel takes full width.
        draw_conversation_panel(frame, area, app);
    } else {
        // Normal: sidebar | conversation
        let h_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(1)])
            .split(area);

        draw_sidebar(frame, h_chunks[0], app);
        draw_conversation_panel(frame, h_chunks[1], app);
    }

    // Overlays
    match &app.mode {
        Mode::Help => draw_help_overlay(frame, area),
        Mode::Confirm => draw_confirm_overlay(frame, area, app),
        Mode::CreatePrompt => draw_create_prompt_overlay(frame, area, app),
        Mode::RepoSelect => draw_repo_select_overlay(frame, area, app),
        Mode::AgentSelect => draw_agent_select_overlay(frame, area, app),
        Mode::ModifierSelect => draw_modifier_select_overlay(frame, area, app),
        Mode::Input => draw_input_overlay(frame, area, app),
        Mode::PrDetail => draw_pr_detail_overlay(frame, area, app),
        Mode::Normal => {}
    }
}

// ── Sidebar ─────────────────────────────────────────────────

fn draw_sidebar(frame: &mut Frame, area: Rect, app: &DaemonTuiApp) {
    let is_focused = app.focus == Panel::Sidebar;
    let border_style = if is_focused {
        theme::border_active()
    } else {
        theme::border()
    };

    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(border_style);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // header
            Constraint::Length(1), // divider
            Constraint::Min(3),    // worker list
            Constraint::Length(1), // divider
            Constraint::Length(1), // status bar
        ])
        .split(inner);

    draw_sidebar_header(frame, chunks[0], app, is_focused);
    draw_divider(frame, chunks[1]);
    draw_worker_list(frame, chunks[2], app);
    draw_divider(frame, chunks[3]);
    draw_sidebar_status_bar(frame, chunks[4], app);
}

fn draw_sidebar_header(frame: &mut Frame, area: Rect, app: &DaemonTuiApp, is_focused: bool) {
    let count = app.workers.len();
    let dir_name = app
        .work_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "swarm".to_string());

    let (prefix, bg, fg, modifier) = if is_focused {
        (" \u{25b8} ", theme::FOCUS_BG, theme::HONEY, Modifier::BOLD)
    } else {
        (" ", theme::COMB, theme::SMOKE, Modifier::empty())
    };

    let label = format!("{}WORKERS ({})", prefix, count);
    let label_len = label.len();
    let dir_max = (area.width as usize).saturating_sub(label_len + 2);
    let dir_str = truncate_str(&dir_name, dir_max);
    let padding = (area.width as usize).saturating_sub(label_len + dir_str.len());

    let line1 = Line::from(vec![
        Span::styled(label, Style::default().fg(fg).bg(bg).add_modifier(modifier)),
        Span::styled(" ".repeat(padding), Style::default().bg(bg)),
        Span::styled(
            dir_str,
            Style::default()
                .fg(if is_focused {
                    theme::HONEY
                } else {
                    theme::SMOKE
                })
                .bg(bg)
                .add_modifier(modifier),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(line1),
        Rect::new(area.x, area.y, area.width, 1),
    );

    // Line 2: connection status
    let conn_line = if app.connected {
        Line::from(Span::styled(" daemon \u{25cf}", theme::success()))
    } else {
        let spin = SPINNER[(app.tick_count / 4) as usize % SPINNER.len()];
        Line::from(vec![
            Span::styled(format!(" {} ", spin), Style::default().fg(theme::HONEY)),
            Span::styled("connecting...", Style::default().fg(theme::SMOKE)),
        ])
    };
    frame.render_widget(
        Paragraph::new(conn_line),
        Rect::new(area.x, area.y + 1, area.width, 1),
    );
}

fn draw_divider(frame: &mut Frame, area: Rect) {
    let inner = Rect::new(area.x + 1, area.y, area.width.saturating_sub(2), 1);
    let divider = Paragraph::new("\u{2500}".repeat(inner.width as usize))
        .style(Style::default().fg(theme::WAX));
    frame.render_widget(divider, inner);
}

/// Height of a single worker item: 3 rows (id, agent+time, prompt).
fn worker_item_height(_worker: &WorkerInfo, _app: &DaemonTuiApp) -> usize {
    3
}

fn draw_worker_list(frame: &mut Frame, area: Rect, app: &DaemonTuiApp) {
    if app.workers.is_empty() {
        let empty_lines = vec![
            Line::from(""),
            Line::from(Span::styled("  No workers yet.", theme::muted())),
            Line::from(Span::styled(
                "  Press [n] to dispatch your first task.",
                theme::muted(),
            )),
        ];
        frame.render_widget(Paragraph::new(empty_lines), area);
        return;
    }

    let viewport = area.height as usize;

    // Compute cumulative tops for each worker (variable height)
    let item_tops: Vec<usize> = app
        .workers
        .iter()
        .scan(0usize, |acc, w| {
            let top = *acc;
            *acc += worker_item_height(w, app);
            Some(top)
        })
        .collect();
    let total_height: usize = app.workers.iter().map(|w| worker_item_height(w, app)).sum();

    // Compute scroll offset
    let selected_top = item_tops[app.selected];
    let selected_bottom = selected_top + worker_item_height(&app.workers[app.selected], app);
    let mut scroll = app.list_scroll.get();

    if selected_top < scroll {
        scroll = selected_top;
    } else if selected_bottom > scroll + viewport {
        scroll = selected_bottom.saturating_sub(viewport);
    }
    if total_height <= viewport {
        scroll = 0;
    } else {
        scroll = scroll.min(total_height - viewport);
    }
    app.list_scroll.set(scroll);

    let mut render_y = area.y;
    let viewport_bottom = area.y + area.height;

    for (i, worker) in app.workers.iter().enumerate() {
        let item_h = worker_item_height(worker, app);
        let item_top = item_tops[i];
        let item_bottom = item_top + item_h;

        if item_bottom <= scroll {
            continue;
        }
        if render_y >= viewport_bottom {
            break;
        }

        let skip_top = scroll.saturating_sub(item_top);
        let available = (viewport_bottom - render_y) as usize;
        let render_h = (item_h - skip_top).min(available);

        if render_h == 0 {
            continue;
        }

        let is_selected = i == app.selected;

        // Draw worker row (3 or 4 lines, possibly clipped)
        let item_lines = worker_item_height(worker, app);
        let worker_rows = item_lines.saturating_sub(skip_top).min(render_h);
        if worker_rows > 0 {
            let rect = Rect::new(area.x, render_y, area.width, worker_rows as u16);
            draw_worker_row(frame, rect, worker, is_selected, i, app);
            render_y += worker_rows as u16;
        }
    }
}

fn draw_worker_row(
    frame: &mut Frame,
    area: Rect,
    worker: &WorkerInfo,
    selected: bool,
    idx: usize,
    app: &DaemonTuiApp,
) {
    let (status_icon, status_style) = match worker.phase {
        WorkerPhase::Creating | WorkerPhase::Starting => {
            ("\u{25cc}", Style::default().fg(theme::HONEY))
        }
        WorkerPhase::Running => ("\u{25cf}", theme::status_running()),
        WorkerPhase::Waiting => ("\u{25cf}", Style::default().fg(theme::HONEY)),
        WorkerPhase::Completed => ("\u{25c6}", theme::status_done()),
        WorkerPhase::Failed => ("\u{2717}", Style::default().fg(Color::Rgb(200, 60, 60))),
    };

    let wt_color = SIDEBAR_COLORS[idx % SIDEBAR_COLORS.len()];

    let row_style = if selected {
        Style::default().bg(Color::Rgb(58, 50, 42))
    } else {
        Style::default().bg(theme::COMB)
    };

    frame.render_widget(Paragraph::new("").style(row_style), area);

    if area.height < 1 {
        return;
    }

    let row_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    // Line 1: color bar + selector + status + name + PR#
    let selector = if selected { "\u{25b8}" } else { " " };
    let selector_style = if selected {
        theme::selected()
    } else {
        theme::muted()
    };

    let mut line1_spans = vec![
        Span::styled("\u{258c}", Style::default().fg(wt_color)),
        Span::styled(selector, selector_style),
        Span::styled(format!("{} ", status_icon), status_style),
        Span::styled(
            truncate_str(&worker.id, (area.width as usize).saturating_sub(12)),
            if selected {
                theme::selected()
            } else {
                theme::text()
            },
        ),
    ];

    if let Some(ref pr_url) = worker.pr_url {
        // Extract PR number from URL
        let pr_num = pr_url.rsplit('/').next().unwrap_or("PR");
        line1_spans.push(Span::styled(
            format!(" #{}", pr_num),
            Style::default().fg(theme::MINT),
        ));
    }

    if row_chunks[0].height > 0 {
        frame.render_widget(Paragraph::new(Line::from(line1_spans)), row_chunks[0]);
    }

    // Line 2: agent label + time
    if area.height >= 2 && row_chunks[1].height > 0 {
        let elapsed_str = if let Some(created) = &worker.created_at {
            let elapsed = Local::now().signed_duration_since(*created);
            if elapsed.num_minutes() < 1 {
                "now".to_string()
            } else if elapsed.num_minutes() < 60 {
                format!("{}m", elapsed.num_minutes())
            } else {
                format!("{}h", elapsed.num_hours())
            }
        } else {
            "?".to_string()
        };

        let conv_info = app.conversations.get(&worker.id);
        let tool_count = conv_info.map(|c| c.tool_count).unwrap_or(0);
        let tool_str = if tool_count > 0 {
            format!(" \u{00b7} {}t", tool_count)
        } else {
            String::new()
        };

        let line2 = Line::from(vec![
            Span::styled("\u{258c}", Style::default().fg(wt_color)),
            Span::styled("  ", Style::default()),
            Span::styled(
                match worker.agent.as_str() {
                    "claude-tui" => "claude",
                    "codex" => "codex",
                    "gemini" => "gemini",
                    "claude" => "claude code",
                    other => other,
                },
                theme::agent_color(),
            ),
            Span::styled(
                format!(" \u{00b7} {}{}", elapsed_str, tool_str),
                Style::default().fg(Color::Rgb(80, 77, 70)),
            ),
        ]);
        frame.render_widget(Paragraph::new(line2), row_chunks[1]);
    }

    // Line 3: truncated prompt
    if area.height >= 3 && row_chunks[2].height > 0 {
        let max_len = (area.width as usize).saturating_sub(3);
        let line3 = Line::from(vec![
            Span::styled("\u{258c}", Style::default().fg(wt_color)),
            Span::styled("  ", Style::default()),
            Span::styled(
                truncate_str(&worker.prompt, max_len),
                Style::default().fg(Color::Rgb(90, 87, 80)),
            ),
        ]);
        frame.render_widget(Paragraph::new(line3), row_chunks[2]);
    }
}

fn draw_sidebar_status_bar(frame: &mut Frame, area: Rect, app: &DaemonTuiApp) {
    if let Some(msg) = app.current_status() {
        let style = if msg.starts_with("error") {
            theme::error()
        } else {
            theme::accent()
        };
        let status = Paragraph::new(format!(" {}", msg)).style(style);
        frame.render_widget(status, area);
    } else {
        let hints = Line::from(vec![
            Span::styled(" n", theme::key_hint()),
            Span::styled(" new  ", theme::key_desc()),
            Span::styled("x", theme::key_hint()),
            Span::styled(" close  ", theme::key_desc()),
            Span::styled("\u{21b5}", theme::key_hint()),
            Span::styled(" open  ", theme::key_desc()),
            Span::styled("?", theme::key_hint()),
            Span::styled(" help  ", theme::key_desc()),
            Span::styled("q", theme::key_hint()),
            Span::styled(" quit", theme::key_desc()),
        ]);
        frame.render_widget(Paragraph::new(hints), area);
    }
}

// ── Conversation Panel ──────────────────────────────────────

fn draw_conversation_panel(frame: &mut Frame, area: Rect, app: &mut DaemonTuiApp) {
    let is_focused = app.focus == Panel::Conversation;
    let border_style = if is_focused {
        theme::border_active()
    } else {
        theme::border()
    };

    if app.workers.is_empty() {
        // Draw unfocused header even with no workers
        draw_conversation_header(
            frame,
            Rect::new(area.x, area.y, area.width, 1),
            None,
            is_focused,
            "",
        );
        let rest = Rect::new(
            area.x,
            area.y + 1,
            area.width,
            area.height.saturating_sub(1),
        );
        let empty_lines = vec![
            Line::from(""),
            Line::from(Span::styled("  No workers yet.", theme::muted())),
            Line::from(Span::styled(
                "  Press [n] to dispatch your first task.",
                theme::muted(),
            )),
        ];
        frame.render_widget(Paragraph::new(empty_lines), rest);
        return;
    }

    let selected_id = app.selected_worker().map(|w| w.id.clone());
    let selected_worker = app.selected_worker().cloned();

    // Determine if we need an input bar
    let show_input = is_focused && app.mode == Mode::Input;

    // Compute input height: 2 (borders) + wrapped line count, capped at 8
    let input_height = if show_input {
        let inner_width = area.width.saturating_sub(4) as usize; // borders + padding
        let text_lines = if inner_width > 0 {
            let char_count = app.input_buffer.len();
            ((char_count / inner_width.max(1)) + 1).min(6) as u16
        } else {
            1
        };
        text_lines + 2 // borders
    } else {
        0
    };

    // Header is 2 lines when selected worker has a PR URL, else 1
    let header_height = if let Some(ref id) = selected_id
        && let Some(w) = app.workers.iter().find(|w| w.id == *id)
        && w.pr_url.is_some()
    {
        2
    } else {
        1
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height), // header bar
            Constraint::Min(1),                // conversation
            Constraint::Length(input_height),  // input
            Constraint::Length(1),             // status bar
        ])
        .split(area);

    // Header bar — compute scroll position for display
    let scroll_pos = if let Some(ref id) = selected_id
        && let Some(conv) = app.conversations.get(id)
    {
        if conv.auto_scroll || conv.total_visual_lines == 0 {
            String::new()
        } else {
            let vh = conv.conversation_area.height as u32;
            let max = conv.total_visual_lines.saturating_sub(vh);
            if max == 0 {
                String::new()
            } else if conv.scroll_offset >= max {
                "Top".to_string()
            } else {
                let pct = 100 - (conv.scroll_offset * 100 / max);
                format!("{}%", pct)
            }
        }
    } else {
        String::new()
    };
    draw_conversation_header(
        frame,
        chunks[0],
        selected_worker.as_ref(),
        is_focused,
        &scroll_pos,
    );

    // Conversation area
    {
        let block = Block::default().borders(Borders::NONE);
        let inner = block.inner(chunks[1]);
        app.viewport_height = inner.height;

        if let Some(ref id) = selected_id {
            if let Some(conv) = app.conversations.get_mut(id) {
                draw_conversation_entries(frame, chunks[1], conv, &block, is_focused);
            } else {
                frame.render_widget(block, chunks[1]);
                let msg = Paragraph::new("  No events yet.").style(theme::muted());
                frame.render_widget(msg, inner);
            }
        } else {
            frame.render_widget(block, chunks[1]);
        }
    }

    // Input bar
    if show_input {
        draw_conversation_input(frame, chunks[2], app);
    }

    // Status bar
    draw_conversation_status_bar(frame, chunks[3], app, border_style);
}

fn draw_conversation_header(
    frame: &mut Frame,
    area: Rect,
    worker: Option<&WorkerInfo>,
    is_focused: bool,
    scroll_pos: &str,
) {
    let (prefix, bg, fg, modifier) = if is_focused {
        (" \u{25b8} ", theme::FOCUS_BG, theme::HONEY, Modifier::BOLD)
    } else {
        (
            "   ",
            Color::Rgb(48, 44, 38),
            theme::SMOKE,
            Modifier::empty(),
        )
    };

    let base_style = Style::default().fg(fg).bg(bg).add_modifier(modifier);

    let (left_text, right_text) = if let Some(w) = worker {
        (
            format!("{}{}", prefix, w.id),
            if scroll_pos.is_empty() {
                String::new()
            } else {
                format!("{}  ", scroll_pos)
            },
        )
    } else {
        (format!("{}no worker selected", prefix), String::new())
    };

    let left_len = left_text.len();
    let right_len = right_text.len();
    let padding = (area.width as usize).saturating_sub(left_len + right_len);

    // Line 1: worker name
    let line1 = Line::from(vec![
        Span::styled(left_text, base_style),
        Span::styled(" ".repeat(padding), Style::default().bg(bg)),
        Span::styled(right_text, base_style),
    ]);

    // Line 2: PR URL (if present and area has room)
    if area.height >= 2
        && let Some(w) = worker
        && let Some(ref pr_url) = w.pr_url
    {
        let url_prefix = "   ";
        let max_url_len = (area.width as usize).saturating_sub(url_prefix.len());
        let display_url = truncate_str(pr_url, max_url_len);
        let url_len = url_prefix.len() + display_url.len();
        let url_padding = (area.width as usize).saturating_sub(url_len);

        let line2 = Line::from(vec![
            Span::styled(url_prefix, Style::default().bg(bg)),
            Span::styled(display_url, Style::default().fg(theme::MINT).bg(bg)),
            Span::styled(" ".repeat(url_padding), Style::default().bg(bg)),
        ]);

        let text = Text::from(vec![line1, line2]);
        frame.render_widget(Paragraph::new(text), area);
    } else {
        frame.render_widget(Paragraph::new(line1), area);
    }
}

fn draw_conversation_entries(
    frame: &mut Frame,
    area: Rect,
    conv: &mut WorkerConversation,
    block: &Block<'_>,
    _is_focused: bool,
) {
    let inner = block.inner(area);
    let mut lines: Vec<Line<'_>> = Vec::new();
    let mut entry_line_map: Vec<(u32, u32)> = Vec::with_capacity(conv.entries.len());

    // Top padding
    lines.push(Line::from(""));

    let mut last_shown_ts = String::new();
    for (i, entry) in conv.entries.iter().enumerate() {
        // In filter mode, skip noise tool calls (unless they errored)
        if conv.filter_noise
            && let ConversationEntry::ToolCall { tool, is_error, .. } = entry
            && is_noise_tool(tool)
            && !*is_error
        {
            entry_line_map.push((lines.len() as u32, 0));
            continue;
        }

        let start = lines.len() as u32;
        let is_focused_tool = conv.focused_tool == Some(i);

        match entry {
            ConversationEntry::User { text, timestamp } => {
                if i > 0 {
                    lines.push(Line::from(""));
                    let w = inner.width as usize;
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
                            .bg(theme::FOCUS_BG)
                            .add_modifier(Modifier::BOLD),
                    ),
                    ts_span,
                ]));
                for line in text.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("  {}", line),
                        Style::default().fg(theme::HONEY).bg(theme::FOCUS_BG),
                    )));
                }
            }
            ConversationEntry::AssistantText { text, timestamp } => {
                let in_same_turn = is_continuation_of_assistant_turn(&conv.entries, i);
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
                let focus_prefix = if is_focused_tool { "\u{25b6} " } else { "  " };
                let focus_bg = if is_focused_tool {
                    theme::TOOL_FOCUS_BG
                } else {
                    theme::COMB
                };

                // Categorize tools for visual hierarchy
                let is_mutation = matches!(tool.as_str(), "Write" | "Edit" | "NotebookEdit");
                let is_execution = matches!(tool.as_str(), "Bash" | "Task" | "Skill");
                let is_noise = matches!(
                    tool.as_str(),
                    "Read" | "Glob" | "Grep" | "WebFetch" | "WebSearch" | "TodoRead" | "TodoWrite"
                );

                // #1: Color-code tool names by category
                let tool_name_color = if is_focused_tool {
                    theme::HONEY
                } else if is_mutation {
                    theme::NECTAR // amber/orange — draws attention
                } else if is_execution {
                    theme::MINT // green — action
                } else if is_noise {
                    Color::Rgb(80, 77, 70) // very dim
                } else {
                    theme::ICE // default
                };

                let tool_style = Style::default()
                    .fg(tool_name_color)
                    .bg(focus_bg)
                    .add_modifier(Modifier::BOLD);

                if *collapsed {
                    // Line 1: icon + tool name + input summary
                    let (icon, icon_style) = if output.is_none() {
                        ("\u{22ef}", theme::muted().bg(focus_bg))
                    } else if *is_error {
                        ("\u{2716}", theme::error().bg(focus_bg))
                    } else {
                        ("\u{2714}", theme::success().bg(focus_bg))
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

                    // #3: Dim noise tools, #4: bold input for mutations
                    let input_style = if is_noise {
                        Style::default().fg(Color::Rgb(70, 67, 60)).bg(focus_bg)
                    } else if is_mutation {
                        Style::default().fg(theme::FROST).bg(focus_bg)
                    } else {
                        theme::muted().bg(focus_bg)
                    };

                    lines.push(Line::from(vec![
                        Span::styled(format!("{}{} ", focus_prefix, icon), icon_style),
                        Span::styled(tool.as_str(), tool_style),
                        Span::styled(format!("  {}{}", preview, ellipsis), input_style),
                    ]));
                    // Line 2: result hint (first useful output line or URL)
                    if let Some(out) = output {
                        // Prefer a line containing a URL, else first non-empty line
                        let hint_line = out
                            .lines()
                            .find(|l| l.contains("https://"))
                            .or_else(|| out.lines().find(|l| !l.trim().is_empty()));
                        if let Some(hint) = hint_line {
                            let has_url = hint.contains("https://");
                            // #2: Blue for URLs, #5: Red for errors, dim for noise
                            let hint_style = if *is_error {
                                theme::error()
                            } else if has_url {
                                Style::default().fg(theme::FROST)
                            } else if is_noise {
                                Style::default().fg(Color::Rgb(60, 57, 50))
                            } else {
                                Style::default().fg(Color::Rgb(90, 87, 80))
                            };
                            let total_out_lines = out.lines().count();
                            let suffix = if total_out_lines > 1 {
                                format!("  ({} more lines)", total_out_lines - 1)
                            } else {
                                String::new()
                            };
                            let hint_truncated: String = hint.chars().take(80).collect();
                            lines.push(Line::from(vec![
                                Span::styled("       \u{2192} ", Style::default().fg(theme::STEEL)),
                                Span::styled(hint_truncated, hint_style),
                                Span::styled(suffix, theme::muted()),
                            ]));
                        }
                    }
                } else {
                    // Expanded view
                    lines.push(Line::from(vec![
                        Span::styled(focus_prefix, theme::muted().bg(focus_bg)),
                        Span::styled(format!(" {} ", tool), tool_style),
                        Span::styled(
                            " \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
                            Style::default().fg(theme::STEEL).bg(focus_bg),
                        ),
                    ]));
                    // Show all input lines (typically short)
                    for line in input.lines() {
                        lines.push(Line::from(Span::styled(
                            format!("  \u{2502} {}", line),
                            Style::default().fg(theme::SLATE),
                        )));
                    }
                    if let Some(out) = output {
                        let out_style = if *is_error {
                            theme::error()
                        } else {
                            theme::muted()
                        };
                        lines.push(Line::from(Span::styled(
                            "  \u{251c}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
                            Style::default().fg(theme::STEEL),
                        )));
                        let out_lines: Vec<&str> = out.lines().collect();
                        let max_output = 50;
                        for line in out_lines.iter().take(max_output) {
                            lines.push(Line::from(Span::styled(
                                format!("  \u{2502} {}", line),
                                out_style,
                            )));
                        }
                        if out_lines.len() > max_output {
                            lines.push(Line::from(Span::styled(
                                format!(
                                    "  \u{2502} ... ({} more lines)",
                                    out_lines.len() - max_output
                                ),
                                theme::muted(),
                            )));
                        }
                    }
                    lines.push(Line::from(Span::styled(
                        "  \u{2514}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
                        Style::default().fg(theme::STEEL),
                    )));
                }
            }
            ConversationEntry::Question { text, timestamp } => {
                let in_same_turn = is_continuation_of_assistant_turn(&conv.entries, i);
                if !in_same_turn {
                    lines.push(Line::from(""));
                    let ts_span = dedup_timestamp(timestamp, &mut last_shown_ts);
                    lines.push(Line::from(vec![
                        Span::styled(
                            "  \u{2753} Claude:",
                            Style::default()
                                .fg(theme::HONEY)
                                .add_modifier(Modifier::BOLD),
                        ),
                        ts_span,
                    ]));
                }
                lines.extend(markdown::render_markdown(text));
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

    // Streaming text
    if !conv.streaming_text.is_empty() {
        let need_header = !is_continuation_of_assistant_turn(&conv.entries, conv.entries.len());
        if need_header {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  Claude:",
                Style::default()
                    .fg(theme::FROST)
                    .add_modifier(Modifier::BOLD),
            )));
        }
        for line in conv.streaming_text.lines() {
            lines.push(Line::from(Span::styled(
                format!("  {}", line),
                theme::text(),
            )));
        }
        if conv.is_streaming {
            lines.push(Line::from(Span::styled(
                "  \u{258c}",
                Style::default()
                    .fg(theme::FROST)
                    .add_modifier(Modifier::BOLD),
            )));
        }
    } else if conv.is_streaming {
        lines.push(Line::from(Span::styled(
            "  \u{258c}",
            Style::default()
                .fg(theme::FROST)
                .add_modifier(Modifier::BOLD),
        )));
    }

    // Bottom padding
    lines.push(Line::from(""));
    lines.push(Line::from(""));

    conv.entry_line_map = entry_line_map;
    conv.total_rendered_lines = lines.len() as u32;
    conv.conversation_area = inner;

    // Compute visual (wrapped) line counts for scroll accuracy
    let inner_width = inner.width.max(1) as usize;
    let mut entry_visual_line_map: Vec<(u32, u32)> = Vec::with_capacity(conv.entry_line_map.len());
    let mut visual_offset: u32 = 0;
    for &(raw_start, raw_count) in &conv.entry_line_map {
        let mut visual_count: u32 = 0;
        for idx in raw_start..(raw_start + raw_count) {
            if let Some(line) = lines.get(idx as usize) {
                let w = line.width();
                visual_count += (w.max(1).div_ceil(inner_width)) as u32;
            } else {
                visual_count += 1;
            }
        }
        entry_visual_line_map.push((visual_offset, visual_count));
        visual_offset += visual_count;
    }
    // Also account for streaming text lines after entries
    let entry_raw_end = conv.entry_line_map.last().map(|(s, c)| s + c).unwrap_or(0) as usize;
    for idx in entry_raw_end..lines.len() {
        if let Some(line) = lines.get(idx) {
            let w = line.width();
            visual_offset += (w.max(1).div_ceil(inner_width)) as u32;
        } else {
            visual_offset += 1;
        }
    }
    conv.entry_visual_line_map = entry_visual_line_map;
    conv.total_visual_lines = visual_offset;

    let visible_height = inner.height as u32;

    if conv.auto_scroll {
        // Auto-scroll: show the bottom of the conversation.
        // Trim to a tail for performance, then use ratatui's own line_count()
        // for exact scroll calculation (our manual width estimate drifts with
        // word wrapping and causes the bottom to get clipped).
        let keep_lines = (visible_height as usize) * 4 + 50;
        let display_lines = if lines.len() > keep_lines {
            &lines[lines.len() - keep_lines..]
        } else {
            &lines[..]
        };
        let paragraph =
            Paragraph::new(Text::from(display_lines.to_vec())).wrap(Wrap { trim: false });
        let actual_lines = paragraph.line_count(inner.width) as u32;
        let scroll = actual_lines.saturating_sub(visible_height);
        let paragraph = paragraph
            .scroll((scroll.min(u16::MAX as u32) as u16, 0))
            .block(block.clone());
        frame.render_widget(paragraph, area);
    } else {
        // Manual scroll: compute exact scroll using ratatui's line_count().
        let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
        let total_visual = paragraph.line_count(inner.width) as u32;
        // Update stored total so scroll_up/scroll_down clamp correctly
        conv.total_visual_lines = total_visual;
        let target_scroll = total_visual
            .saturating_sub(visible_height)
            .saturating_sub(conv.scroll_offset);

        let paragraph = paragraph
            .scroll((target_scroll.min(u16::MAX as u32) as u16, 0))
            .block(block.clone());
        frame.render_widget(paragraph, area);
    }
}

fn draw_conversation_input(frame: &mut Frame, area: Rect, app: &DaemonTuiApp) {
    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border_active())
        .title(Span::styled(" Message ", theme::accent()))
        .padding(Padding::horizontal(1));

    let input_text = Paragraph::new(app.input_buffer.as_str())
        .style(theme::text())
        .wrap(Wrap { trim: false })
        .block(input_block);

    frame.render_widget(input_text, area);

    // Compute cursor position accounting for wrapping
    let inner_width = area.width.saturating_sub(4) as usize; // borders + padding
    if let Some(cursor_row) = app.input_cursor.checked_div(inner_width) {
        let cursor_col = app.input_cursor % inner_width;
        frame.set_cursor_position((
            area.x + 2 + cursor_col as u16,
            area.y + 1 + cursor_row as u16,
        ));
    } else {
        frame.set_cursor_position((area.x + 2, area.y + 1));
    }
}

fn draw_conversation_status_bar(
    frame: &mut Frame,
    area: Rect,
    app: &DaemonTuiApp,
    _border_style: Style,
) {
    let spin = SPINNER[(app.tick_count / 4) as usize % SPINNER.len()];

    // Get current worker phase info
    let (phase_text, phase_style) = if let Some(worker) = app.selected_worker() {
        match worker.phase {
            WorkerPhase::Creating | WorkerPhase::Starting => {
                (format!("{} starting...", spin), theme::status_running())
            }
            WorkerPhase::Running => (format!("{} running...", spin), theme::status_running()),
            WorkerPhase::Waiting => {
                let dot = if (app.tick_count / 8).is_multiple_of(2) {
                    "\u{25cb}"
                } else {
                    "\u{25cf}"
                };
                (
                    format!("{} waiting", dot),
                    Style::default().fg(theme::HONEY),
                )
            }
            WorkerPhase::Completed => ("\u{25cf} done".to_string(), theme::status_done()),
            WorkerPhase::Failed => ("\u{2717} failed".to_string(), theme::error()),
        }
    } else {
        ("no worker selected".to_string(), theme::muted())
    };

    let conv_info = app
        .selected_worker()
        .and_then(|w| app.conversations.get(&w.id));
    let right_info = if let Some(conv) = conv_info {
        let cost_str = conv
            .cost_usd
            .map(|c| format!(" ${:.4}", c))
            .unwrap_or_default();
        format!(
            "tools: {} | turns: {}{}",
            conv.tool_count, conv.turn_count, cost_str
        )
    } else {
        String::new()
    };

    let is_filtered = app.selected_conversation().is_some_and(|c| c.filter_noise);

    let hints = if app.focus == Panel::Conversation {
        if matches!(
            app.selected_worker().map(|w| &w.phase),
            Some(WorkerPhase::Waiting)
        ) {
            " j/k:scroll c:tools f:filter i:input tab:sidebar "
        } else {
            " j/k:scroll c:tools f:filter tab:sidebar "
        }
    } else {
        ""
    };

    let filter_badge = if is_filtered { " [filtered]" } else { "" };

    let scroll_hint = if let Some(conv) = app
        .selected_worker()
        .and_then(|w| app.conversations.get(&w.id))
    {
        if !conv.auto_scroll && conv.scroll_offset > 0 {
            format!("↑{} ", conv.scroll_offset)
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    let available = area.width as usize;
    let left = format!(" [{}]{}", phase_text, filter_badge);
    let right = format!("{}{}  {} ", scroll_hint, hints, right_info);
    let padding = available.saturating_sub(left.len() + right.len());

    let line = Line::from(vec![
        Span::styled(left, phase_style),
        Span::styled(" ".repeat(padding), theme::muted()),
        Span::styled(right, theme::muted()),
    ]);

    let bar = Paragraph::new(line).style(Style::default().bg(theme::COMB));
    frame.render_widget(bar, area);
}

// ── Overlays ─────────────────────────────────────────────────

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

fn draw_help_overlay(frame: &mut Frame, area: Rect) {
    let popup_width = (area.width).min(50);
    let popup_height = (area.height).min(30);
    let popup = centered_rect(popup_width, popup_height, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(" help ", theme::title()))
        .borders(Borders::ALL)
        .border_style(theme::border_active())
        .style(Style::default().bg(theme::COMB));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let sections = vec![
        ("", "── Sidebar ──"),
        ("j/k", "navigate workers"),
        ("tab/l/\u{21b5}", "focus conversation"),
        ("n", "new worker"),
        ("x", "close worker"),
        ("m", "merge worker"),
        ("p", "PR detail"),
        ("z", "toggle zoom"),
        ("?", "toggle help"),
        ("q", "quit"),
        ("", ""),
        ("", "── Conversation ──"),
        ("tab/h", "focus sidebar"),
        ("[ / ]", "cycle tool focus"),
        ("\u{21b5}", "toggle focused tool"),
        ("esc", "clear focus / sidebar"),
        ("j/k", "scroll"),
        ("PgUp/Dn", "scroll page"),
        ("Home", "scroll to top"),
        ("G/End", "scroll to bottom"),
        ("c", "toggle all tools"),
        ("f", "filter noise tools"),
        ("p", "PR detail"),
        ("i/s", "enter input mode"),
        ("z", "toggle zoom"),
    ];

    for (i, (key, desc)) in sections.iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + inner.height {
            break;
        }
        if key.is_empty() {
            let line = Line::from(Span::styled(*desc, theme::muted()));
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(inner.x + 1, y, inner.width.saturating_sub(2), 1),
            );
        } else {
            let line = Line::from(vec![
                Span::styled(format!("  {:>7} ", key), theme::key_hint()),
                Span::styled(*desc, theme::key_desc()),
            ]);
            frame.render_widget(Paragraph::new(line), Rect::new(inner.x, y, inner.width, 1));
        }
    }
}

fn draw_confirm_overlay(frame: &mut Frame, area: Rect, app: &DaemonTuiApp) {
    let popup_width = (area.width).min(46);
    let popup = centered_rect(popup_width, 5, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(" confirm ", theme::title()))
        .borders(Borders::ALL)
        .border_style(theme::border_active())
        .style(Style::default().bg(theme::COMB));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let msg = Paragraph::new(app.confirm_message.as_str())
        .style(theme::text())
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true });
    let msg_area = Rect::new(inner.x, inner.y + 1, inner.width, 2);
    frame.render_widget(msg, msg_area);
}

fn draw_create_prompt_overlay(frame: &mut Frame, area: Rect, app: &DaemonTuiApp) {
    let popup_width = (area.width).min(60);
    let popup = centered_rect(popup_width, 12, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(" task ", theme::title()))
        .borders(Borders::ALL)
        .border_style(theme::border_active())
        .style(Style::default().bg(theme::COMB));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let buf_lines: Vec<&str> = app.input_buffer.split('\n').collect();
    let mut cursor_line = 0usize;
    let mut cursor_col = 0usize;
    let mut pos = 0usize;
    for (i, line) in buf_lines.iter().enumerate() {
        let line_chars = line.chars().count();
        if pos + line_chars >= app.input_cursor && i < buf_lines.len() {
            cursor_line = i;
            cursor_col = app.input_cursor - pos;
            break;
        }
        pos += line_chars + 1;
    }

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

    let input_height = inner.height.saturating_sub(1);
    let input_area = Rect::new(inner.x, inner.y, inner.width, input_height);

    let text = Text::from(styled_lines);
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), input_area);

    let hint = Line::from(vec![
        Span::styled("\u{21b5}", theme::key_hint()),
        Span::styled(" submit  ", theme::key_desc()),
        Span::styled("alt+\u{21b5}", theme::key_hint()),
        Span::styled(" newline  ", theme::key_desc()),
        Span::styled("esc", theme::key_hint()),
        Span::styled(" cancel", theme::key_desc()),
    ]);
    let hint_area = Rect::new(
        inner.x + 1,
        inner.y + inner.height - 1,
        inner.width.saturating_sub(2),
        1,
    );
    frame.render_widget(Paragraph::new(hint), hint_area);
}

fn draw_repo_select_overlay(frame: &mut Frame, area: Rect, app: &DaemonTuiApp) {
    let repos = &app.repos;
    let popup_width = (area.width).min(44);
    let popup_height = (repos.len() as u16 + 4).min(area.height);
    let popup = centered_rect(popup_width, popup_height, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(" repo ", theme::title()))
        .borders(Borders::ALL)
        .border_style(theme::border_active())
        .style(Style::default().bg(theme::COMB));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    for (i, repo) in repos.iter().enumerate() {
        let is_selected = i == app.repo_select_index;
        let y = inner.y + 1 + i as u16;
        if y >= inner.y + inner.height {
            break;
        }
        let name = crate::core::git::repo_name(repo);
        let line = Line::from(vec![
            Span::styled(
                if is_selected { " \u{25b8} " } else { "   " },
                if is_selected {
                    theme::selected()
                } else {
                    theme::muted()
                },
            ),
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
}

fn draw_agent_select_overlay(frame: &mut Frame, area: Rect, app: &DaemonTuiApp) {
    let agents = daemon_agents();
    let popup_width = (area.width).min(36);
    let popup = centered_rect(popup_width, (agents.len() as u16) + 4, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(" agent ", theme::title()))
        .borders(Borders::ALL)
        .border_style(theme::border_active())
        .style(Style::default().bg(theme::COMB));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    for (i, agent) in agents.iter().enumerate() {
        let is_selected = i == app.agent_select_index;
        let y = inner.y + 1 + i as u16;

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
                agent.daemon_name(),
                if is_selected {
                    theme::selected()
                } else {
                    theme::text()
                },
            ),
        ]);
        frame.render_widget(Paragraph::new(line), Rect::new(inner.x, y, inner.width, 1));
    }
}

fn draw_modifier_select_overlay(frame: &mut Frame, area: Rect, app: &DaemonTuiApp) {
    let popup_width = (area.width).min(54);
    let popup_height = (app.modifier_prompts.len() as u16 + 6).min(area.height);
    let popup = centered_rect(popup_width, popup_height, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(" modifiers ", theme::title()))
        .borders(Borders::ALL)
        .border_style(theme::border_active())
        .style(Style::default().bg(theme::COMB));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if app.modifier_prompts.is_empty() {
        let empty = Line::from(Span::styled(" no modifiers available", theme::muted()));
        frame.render_widget(
            Paragraph::new(empty),
            Rect::new(inner.x, inner.y + 1, inner.width, 1),
        );
    } else {
        for (i, modifier) in app.modifier_prompts.iter().enumerate() {
            let y = inner.y + 1 + i as u16;
            if y >= inner.y + inner.height.saturating_sub(3) {
                break;
            }
            let is_cursor = i == app.modifier_cursor;
            let is_checked = app.modifier_selected[i];

            let cursor_indicator = if is_cursor { "\u{25b8}" } else { " " };
            let checkbox = if is_checked { "[x]" } else { "[ ]" };

            let cursor_style = if is_cursor {
                theme::selected()
            } else {
                theme::muted()
            };
            let checkbox_style = if is_checked {
                Style::default().fg(theme::HONEY)
            } else {
                theme::muted()
            };
            let label_style = if is_cursor {
                theme::selected()
            } else {
                theme::text()
            };

            let line = Line::from(vec![
                Span::styled(format!(" {} ", cursor_indicator), cursor_style),
                Span::styled(format!("{} ", checkbox), checkbox_style),
                Span::styled(modifier.label(), label_style),
            ]);
            frame.render_widget(Paragraph::new(line), Rect::new(inner.x, y, inner.width, 1));
        }
    }

    // Context line
    let count = app.modifier_selected.iter().filter(|&&s| s).count();
    let ctx = Line::from(vec![
        Span::styled(format!(" selected: {}", count), theme::muted()),
        Span::styled("  (optional, prepended to prompt)", theme::muted()),
    ]);
    let ctx_y = inner.y + inner.height.saturating_sub(3);
    frame.render_widget(
        Paragraph::new(ctx),
        Rect::new(inner.x, ctx_y, inner.width, 1),
    );

    // Hint
    let hint = Line::from(vec![
        Span::styled("space", theme::key_hint()),
        Span::styled(" toggle  ", theme::key_desc()),
        Span::styled("a", theme::key_hint()),
        Span::styled(" all  ", theme::key_desc()),
        Span::styled("\u{21b5}", theme::key_hint()),
        Span::styled(" next  ", theme::key_desc()),
        Span::styled("esc", theme::key_hint()),
        Span::styled(" back", theme::key_desc()),
    ]);
    let hint_y = inner.y + inner.height.saturating_sub(1);
    frame.render_widget(
        Paragraph::new(hint),
        Rect::new(inner.x + 1, hint_y, inner.width.saturating_sub(2), 1),
    );
}

fn draw_input_overlay(frame: &mut Frame, area: Rect, app: &DaemonTuiApp) {
    // Only show if conversation is focused — we draw it inline in the conversation panel.
    // This overlay is for when sidebar is focused and user presses 's' to quick-send.
    if app.focus == Panel::Conversation {
        return; // handled inline
    }

    let popup_width = (area.width).min(60);
    let popup = centered_rect(popup_width, 5, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(" send message ", theme::title()))
        .borders(Borders::ALL)
        .border_style(theme::border_active())
        .style(Style::default().bg(theme::COMB));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let input = Paragraph::new(format!(" > {}", app.input_buffer)).style(theme::text());
    frame.render_widget(input, Rect::new(inner.x, inner.y + 1, inner.width, 1));

    frame.set_cursor_position((inner.x + 3 + app.input_cursor as u16, inner.y + 1));
}

fn draw_pr_detail_overlay(frame: &mut Frame, area: Rect, app: &DaemonTuiApp) {
    let pr = match &app.pr_detail {
        Some(pr) => pr,
        None => return,
    };

    // Size to fit the URL (+ 12 for "  URL:   " prefix and border padding)
    let min_width = (pr.url.len() as u16 + 12).max(56);
    let popup_width = area.width.min(min_width);
    let popup = centered_rect(popup_width, 11, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(format!(" PR #{} ", pr.number), theme::title()))
        .borders(Borders::ALL)
        .border_style(theme::border_active())
        .style(Style::default().bg(theme::COMB));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let state_style = match pr.state.as_str() {
        "MERGED" => Style::default().fg(Color::Rgb(160, 100, 220)),
        "CLOSED" => Style::default().fg(Color::Rgb(200, 60, 60)),
        _ => Style::default().fg(Color::Rgb(60, 180, 60)),
    };

    let lines = vec![
        Line::from(vec![
            Span::styled("  Title: ", theme::muted()),
            Span::styled(
                truncate_str(&pr.title, (inner.width as usize).saturating_sub(10)),
                theme::text(),
            ),
        ]),
        Line::from(vec![
            Span::styled("  State: ", theme::muted()),
            Span::styled(&pr.state, state_style),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  URL:   ", theme::muted()),
            Span::styled(&pr.url, Style::default().fg(theme::FROST)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("    o", theme::key_hint()),
            Span::styled(" open  ", theme::key_desc()),
            Span::styled("c", theme::key_hint()),
            Span::styled(" copy  ", theme::key_desc()),
            Span::styled("esc", theme::key_hint()),
            Span::styled(" dismiss", theme::key_desc()),
        ]),
    ];

    let text = Paragraph::new(lines).wrap(Wrap { trim: false });
    let content_area = Rect::new(
        inner.x,
        inner.y + 1,
        inner.width,
        inner.height.saturating_sub(1),
    );
    frame.render_widget(text, content_area);
}

// ── Helpers ─────────────────────────────────────────────────

/// Check if entry at `idx` is a continuation of a Claude turn (looking past tool calls).
fn is_continuation_of_assistant_turn(entries: &[ConversationEntry], idx: usize) -> bool {
    if idx == 0 {
        return false;
    }
    for j in (0..idx).rev() {
        match &entries[j] {
            ConversationEntry::AssistantText { .. } => return true,
            ConversationEntry::ToolCall { .. } => continue,
            _ => return false,
        }
    }
    false
}

/// Show timestamp only if it differs from the last shown one.
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

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else if max > 1 {
        format!("{}~", &s[..max - 1])
    } else {
        "~".to_string()
    }
}
