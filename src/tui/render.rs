use super::{
    app::{App, Mode, PaneStatus, Worktree},
    theme,
};

use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use chrono::Local;

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();

    // Background
    frame.render_widget(Block::default().style(Style::default().bg(theme::COMB)), area);

    if app.worktrees.is_empty() && app.mode == Mode::Normal {
        draw_welcome(frame, area, app);
    } else {
        draw_sidebar(frame, area, app);
    }

    // Overlays
    match &app.mode {
        Mode::Input => draw_input_overlay(frame, area, app),
        Mode::RepoSelect => draw_repo_select_overlay(frame, area, app),
        Mode::AgentSelect => draw_agent_select_overlay(frame, area, app),
        Mode::Confirm => draw_confirm_overlay(frame, area, app),
        Mode::Help => draw_help_overlay(frame, area),
        Mode::PrDetail => draw_pr_overlay(frame, area, app),
        _ => {}
    }
}

// ── Welcome Screen ─────────────────────────────────────────

fn draw_welcome(frame: &mut Frame, area: Rect, app: &App) {
    let multi_repo = app.repos.len() > 1;
    let repo_list_height = if multi_repo { app.repos.len() as u16 + 1 } else { 0 };

    // Center content horizontally in a 44-col block
    let content_width = 44u16.min(area.width);
    let h_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(content_width),
            Constraint::Min(0),
        ])
        .split(area);
    let center = h_chunks[1];

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),                      // top padding
            Constraint::Length(3),                    // header
            Constraint::Length(1),                    // spacer
            Constraint::Length(1),                    // tagline
            Constraint::Length(1),                    // repo info (single) or label (multi)
            Constraint::Length(repo_list_height),     // repo list (0 if single)
            Constraint::Length(1),                    // spacer
            Constraint::Length(2),                    // error
            Constraint::Length(1),                    // spacer
            Constraint::Length(5),                    // keys
            Constraint::Min(1),                      // bottom padding
        ])
        .split(center);

    // Logo with scattered particles
    let logo = vec![
        Line::from(vec![
            Span::styled("     \u{00b7}  \u{00b7}     \u{00b7}", theme::muted()),
        ]),
        Line::from(vec![
            Span::styled("  \u{00b7}  ", theme::muted()),
            Span::styled("s w a r m", theme::logo()),
        ]),
        Line::from(vec![
            Span::styled("   \u{00b7}    \u{00b7}   \u{00b7}", theme::muted()),
        ]),
    ];
    frame.render_widget(Paragraph::new(logo), chunks[1]);

    // Tagline
    let tagline = Paragraph::new("run agents in parallel")
        .style(theme::muted());
    frame.render_widget(tagline, chunks[3]);

    if multi_repo {
        // Label
        let label = Paragraph::new(format!("{} repos", app.repos.len()))
            .style(theme::accent());
        frame.render_widget(label, chunks[4]);

        // Repo list
        let mut repo_lines: Vec<Line> = Vec::new();
        for repo in &app.repos {
            let name = crate::core::git::repo_name(repo);
            repo_lines.push(Line::from(vec![
                Span::styled("  \u{00b7} ", theme::muted()),
                Span::styled(name, Style::default().fg(theme::SMOKE)),
            ]));
        }
        frame.render_widget(Paragraph::new(repo_lines), chunks[5]);
    } else {
        // Single repo name
        let repo = Paragraph::new(app.repo_display_name())
            .style(theme::accent());
        frame.render_widget(repo, chunks[4]);
    }

    // Error message (if any)
    if let Some(msg) = app.current_status() {
        let style = if msg.starts_with("error") {
            theme::error()
        } else {
            theme::accent()
        };
        let err = Paragraph::new(msg.to_string()).style(style);
        frame.render_widget(err, chunks[7]);
    }

    // Key hints
    let keys = vec![
        Line::from(vec![
            Span::styled("n ", theme::key_hint()),
            Span::styled("new worktree + agent", theme::key_desc()),
        ]),
        Line::from(vec![
            Span::styled("? ", theme::key_hint()),
            Span::styled("help", theme::key_desc()),
        ]),
        Line::from(vec![
            Span::styled("q ", theme::key_hint()),
            Span::styled("quit", theme::key_desc()),
        ]),
    ];
    frame.render_widget(Paragraph::new(keys), chunks[9]);
}

// ── Sidebar Layout ─────────────────────────────────────────

fn draw_sidebar(frame: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // header
            Constraint::Length(1), // divider
            Constraint::Min(3),   // worktree list
            Constraint::Length(1), // divider
            Constraint::Length(1), // status bar
        ])
        .split(area);

    draw_sidebar_header(frame, chunks[0], app);
    draw_divider(frame, chunks[1]);
    draw_worktree_list(frame, chunks[2], app);
    draw_divider(frame, chunks[3]);
    draw_status_bar(frame, chunks[4], app);
}

fn draw_sidebar_header(frame: &mut Frame, area: Rect, app: &App) {
    // Line 1: "swarm (N)   project-name"
    let count = app.worktrees.len();
    let line1 = Line::from(vec![
        Span::styled(" swarm", theme::logo()),
        Span::styled(format!(" ({})", count), theme::muted()),
        Span::styled("  ", Style::default()),
        Span::styled(
            truncate_str(&app.repo_display_name(), (area.width as usize).saturating_sub(16)),
            theme::accent(),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(line1),
        Rect::new(area.x, area.y, area.width, 1),
    );
}

fn draw_divider(frame: &mut Frame, area: Rect) {
    let inner = Rect::new(area.x + 1, area.y, area.width.saturating_sub(2), 1);
    let divider = Paragraph::new("\u{2500}".repeat(inner.width as usize))
        .style(Style::default().fg(theme::WAX));
    frame.render_widget(divider, inner);
}

/// Items in the sidebar — either a repo group header or a worktree row.
enum SidebarItem {
    RepoHeader(String),
    WorktreeRow(usize),
}

/// Build the list of sidebar items. Single repo = flat list (no headers).
/// Multi-repo = group worktrees under repo name headers (only for repos with worktrees).
fn build_sidebar_items(app: &App) -> Vec<SidebarItem> {
    let multi_repo = app.repos.len() > 1;
    if !multi_repo {
        return app
            .worktrees
            .iter()
            .enumerate()
            .map(|(i, _)| SidebarItem::WorktreeRow(i))
            .collect();
    }

    // Group worktrees by repo_path, preserving order of first appearance
    let mut seen_repos: Vec<std::path::PathBuf> = Vec::new();
    for wt in &app.worktrees {
        if !seen_repos.contains(&wt.repo_path) {
            seen_repos.push(wt.repo_path.clone());
        }
    }

    let mut items = Vec::new();
    for repo in &seen_repos {
        let repo_name = crate::core::git::repo_name(repo);
        items.push(SidebarItem::RepoHeader(repo_name));
        for (i, wt) in app.worktrees.iter().enumerate() {
            if wt.repo_path == *repo {
                items.push(SidebarItem::WorktreeRow(i));
            }
        }
    }
    items
}

fn draw_worktree_list(frame: &mut Frame, area: Rect, app: &App) {
    if app.worktrees.is_empty() {
        let empty = Paragraph::new(" no worktrees yet\n press n to start")
            .style(theme::muted());
        frame.render_widget(empty, area);
        return;
    }

    let items = build_sidebar_items(app);
    let viewport = area.height as usize;

    // Compute item heights and cumulative top positions
    let heights: Vec<usize> = items
        .iter()
        .map(|item| match item {
            SidebarItem::RepoHeader(_) => 1,
            SidebarItem::WorktreeRow(_) => 3,
        })
        .collect();

    let mut tops = Vec::with_capacity(items.len());
    let mut cumulative = 0usize;
    for &h in &heights {
        tops.push(cumulative);
        cumulative += h;
    }
    let total_height = cumulative;

    // Find the sidebar item index for the currently selected worktree
    let selected_item = items
        .iter()
        .position(|item| matches!(item, SidebarItem::WorktreeRow(idx) if *idx == app.selected))
        .unwrap_or(0);

    // Adjust scroll offset to keep the selected item fully visible
    let mut scroll = app.list_scroll.get();
    let sel_top = tops[selected_item];
    let sel_bottom = sel_top + heights[selected_item];

    if sel_top < scroll {
        scroll = sel_top;
    } else if sel_bottom > scroll + viewport {
        scroll = sel_bottom.saturating_sub(viewport);
    }

    // Clamp scroll to valid range
    if total_height <= viewport {
        scroll = 0;
    } else {
        scroll = scroll.min(total_height - viewport);
    }

    app.list_scroll.set(scroll);

    // Render visible items with their positions offset by scroll
    let mut render_y = area.y;
    let viewport_bottom = area.y + area.height;

    for (i, item) in items.iter().enumerate() {
        let item_top = tops[i];
        let item_bottom = item_top + heights[i];

        // Skip items fully above the scroll viewport
        if item_bottom <= scroll {
            continue;
        }
        // Stop if we've filled the viewport
        if render_y >= viewport_bottom {
            break;
        }

        // Lines clipped from the top of this item (for partial visibility)
        let skip_top = scroll.saturating_sub(item_top);
        let available = (viewport_bottom - render_y) as usize;
        let render_h = (heights[i] - skip_top).min(available);

        if render_h == 0 {
            continue;
        }

        let rect = Rect::new(area.x, render_y, area.width, render_h as u16);
        render_y += render_h as u16;

        match item {
            SidebarItem::RepoHeader(name) => {
                draw_repo_header(frame, rect, name);
            }
            SidebarItem::WorktreeRow(idx) => {
                let wt = &app.worktrees[*idx];
                let is_selected = *idx == app.selected;
                draw_worktree_row(frame, rect, wt, is_selected, *idx);
            }
        }
    }
}

fn draw_repo_header(frame: &mut Frame, area: Rect, name: &str) {
    let line = Line::from(vec![
        Span::styled(" ", Style::default()),
        Span::styled(
            name.to_uppercase(),
            Style::default()
                .fg(Color::Rgb(120, 117, 110))
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// Bright sidebar colors for the left bar indicator.
const SIDEBAR_COLORS: &[Color] = &[
    Color::Rgb(180, 120, 60),  // warm brown
    Color::Rgb(60, 120, 180),  // cool blue
    Color::Rgb(60, 180, 60),   // forest green
    Color::Rgb(140, 60, 180),  // purple
    Color::Rgb(60, 180, 180),  // teal
    Color::Rgb(180, 150, 60),  // amber
    Color::Rgb(180, 60, 120),  // rose
    Color::Rgb(100, 180, 60),  // olive
];


fn draw_worktree_row(frame: &mut Frame, area: Rect, wt: &Worktree, selected: bool, idx: usize) {
    let status = wt.status();
    let status_icon = match status {
        PaneStatus::Running => "\u{25cf}",
        PaneStatus::Done => "\u{25c6}",
    };
    let status_style = match status {
        PaneStatus::Running => theme::status_running(),
        PaneStatus::Done => theme::status_done(),
    };

    let wt_color = SIDEBAR_COLORS[idx % SIDEBAR_COLORS.len()];

    let row_style = if selected {
        Style::default().bg(Color::Rgb(58, 50, 42))
    } else {
        Style::default().bg(theme::COMB)
    };

    // Background
    frame.render_widget(Paragraph::new("").style(row_style), area);

    let row_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    // Line 1: color bar + selector + status + name + indicator + PR
    let selector = if selected { "\u{25b8}" } else { " " };
    let selector_style = if selected {
        theme::selected()
    } else {
        theme::muted()
    };

    let mut line1_spans = vec![
        Span::styled("\u{258c}", Style::default().fg(wt_color)), // colored left bar
        Span::styled(selector, selector_style),
        Span::styled(format!("{} ", status_icon), status_style),
        Span::styled(
            truncate_str(&wt.id, (area.width as usize).saturating_sub(18)),
            if selected {
                theme::selected()
            } else {
                theme::text()
            },
        ),
        Span::styled(
            format!(" {}", wt.window_indicator()),
            Style::default().fg(Color::Rgb(100, 97, 90)),
        ),
    ];

    // PR badge
    if let Some(ref pr) = wt.pr {
        let pr_badge = format!(" #{}", pr.number);
        let pr_style = match pr.state.as_str() {
            "MERGED" => theme::success(),
            "OPEN" => Style::default().fg(theme::MINT),
            _ => theme::muted(),
        };
        line1_spans.push(Span::styled(pr_badge, pr_style));
    }

    frame.render_widget(Paragraph::new(Line::from(line1_spans)), row_chunks[0]);

    // Line 2: agent label + time
    let elapsed = Local::now().signed_duration_since(wt.created_at);
    let ago = if elapsed.num_minutes() < 1 {
        "now".to_string()
    } else if elapsed.num_minutes() < 60 {
        format!("{}m", elapsed.num_minutes())
    } else {
        format!("{}h", elapsed.num_hours())
    };

    let line2 = Line::from(vec![
        Span::styled("\u{258c}", Style::default().fg(wt_color)), // colored left bar
        Span::styled("  ", Style::default()),
        Span::styled(wt.agent_kind.label(), theme::agent_color()),
        Span::styled(
            format!(" \u{00b7} {}", ago),
            Style::default().fg(Color::Rgb(80, 77, 70)),
        ),
    ]);
    frame.render_widget(Paragraph::new(line2), row_chunks[1]);

    // Line 3: task summary (or truncated prompt as fallback)
    let max_summary_len = (area.width as usize).saturating_sub(3); // bar + 2 spaces
    let (summary_text, summary_style) = if let Some(ref summary) = wt.summary {
        (
            truncate_str(summary, max_summary_len),
            Style::default().fg(Color::Rgb(140, 137, 130)),
        )
    } else if !wt.prompt.is_empty() {
        (
            truncate_str(&wt.prompt, max_summary_len),
            Style::default().fg(Color::Rgb(90, 87, 80)),
        )
    } else {
        (String::new(), theme::muted())
    };

    let line3 = Line::from(vec![
        Span::styled("\u{258c}", Style::default().fg(wt_color)),
        Span::styled("  ", Style::default()),
        Span::styled(summary_text, summary_style),
    ]);
    frame.render_widget(Paragraph::new(line3), row_chunks[2]);
}

fn draw_status_bar(frame: &mut Frame, area: Rect, app: &App) {
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
            Span::styled("t", theme::key_hint()),
            Span::styled(" term  ", theme::key_desc()),
            Span::styled("\u{21b5}", theme::key_hint()),
            Span::styled(" jump  ", theme::key_desc()),
            Span::styled("?", theme::key_hint()),
            Span::styled(" help", theme::key_desc()),
        ]);
        frame.render_widget(Paragraph::new(hints), area);
    }
}

// ── Overlays ───────────────────────────────────────────────

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

fn draw_input_overlay(frame: &mut Frame, area: Rect, app: &App) {
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

    // Split buffer into lines and locate cursor
    let buf_lines: Vec<&str> = app.input_buffer.split('\n').collect();
    let mut cursor_line = 0usize;
    let mut cursor_col = 0usize;
    let mut pos = 0usize;
    for (i, line) in buf_lines.iter().enumerate() {
        let line_chars = line.chars().count();
        if pos + line_chars >= app.input_cursor && i <= buf_lines.len() - 1 {
            cursor_line = i;
            cursor_col = app.input_cursor - pos;
            break;
        }
        pos += line_chars + 1; // +1 for the \n
    }

    // Build styled lines with cursor highlight
    let mut styled_lines: Vec<Line> = Vec::new();
    for (i, line_str) in buf_lines.iter().enumerate() {
        let prefix = if i == 0 { " > " } else { "   " };
        let prefix_style = if i == 0 { theme::accent() } else { theme::text() };

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

    // Reserve 1 row for the hint line
    let input_height = inner.height.saturating_sub(1);
    let input_area = Rect::new(inner.x, inner.y, inner.width, input_height);

    // Calculate scroll to keep cursor visible
    // Approximate visual lines by accounting for wrapping
    let wrap_width = inner.width as usize;
    let mut visual_lines_before_cursor = 0usize;
    for (_i, line_str) in buf_lines.iter().enumerate().take(cursor_line) {
        let prefix_len = 3; // " > " or "   "
        let line_width = prefix_len + line_str.chars().count();
        visual_lines_before_cursor += 1 + line_width.saturating_sub(1) / wrap_width.max(1);
    }
    // Add the cursor line itself (partial)
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

    // Hint
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

/// Label for a repo picker index: 1-9 then a-z.
fn repo_picker_label(i: usize) -> String {
    if i < 9 {
        format!("{}", i + 1)
    } else {
        let c = (b'a' + (i - 9) as u8) as char;
        format!("{}", c)
    }
}

fn draw_repo_select_overlay(frame: &mut Frame, area: Rect, app: &App) {
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
            Span::styled(format!("{} ", repo_picker_label(i)), theme::muted()),
            Span::styled(
                name,
                if is_selected {
                    theme::selected()
                } else {
                    theme::text()
                },
            ),
        ]);

        frame.render_widget(
            Paragraph::new(line),
            Rect::new(inner.x, y, inner.width, 1),
        );
    }
}

fn draw_agent_select_overlay(frame: &mut Frame, area: Rect, app: &App) {
    let agents = crate::core::agent::AgentKind::all();
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
                agent.name(),
                if is_selected {
                    theme::selected()
                } else {
                    theme::text()
                },
            ),
        ]);

        frame.render_widget(
            Paragraph::new(line),
            Rect::new(inner.x, y, inner.width, 1),
        );
    }
}

fn draw_confirm_overlay(frame: &mut Frame, area: Rect, app: &App) {
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

fn draw_help_overlay(frame: &mut Frame, area: Rect) {
    let popup_width = (area.width).min(46);
    let popup_height = (area.height).min(16);
    let popup = centered_rect(popup_width, popup_height, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(" help ", theme::title()))
        .borders(Borders::ALL)
        .border_style(theme::border_active())
        .style(Style::default().bg(theme::COMB));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let keys = vec![
        ("n", "new worktree + agent"),
        ("t", "add terminal pane"),
        ("p", "PR details"),
        ("j/k", "navigate worktrees"),
        ("\u{21b5}", "jump to agent pane"),
        ("m", "merge worktree to base"),
        ("x", "close worktree"),
        ("?", "toggle help"),
        ("q", "quit"),
    ];

    for (i, (key, desc)) in keys.iter().enumerate() {
        let y = inner.y + 1 + i as u16;
        if y >= inner.y + inner.height {
            break;
        }

        let line = Line::from(vec![
            Span::styled(format!("  {:>5} ", key), theme::key_hint()),
            Span::styled(*desc, theme::key_desc()),
        ]);

        frame.render_widget(
            Paragraph::new(line),
            Rect::new(inner.x, y, inner.width, 1),
        );
    }
}

fn draw_pr_overlay(frame: &mut Frame, area: Rect, app: &App) {
    let wt = match app.worktrees.get(app.selected) {
        Some(wt) => wt,
        None => return,
    };
    let pr = match &wt.pr {
        Some(pr) => pr,
        None => return,
    };

    let popup_width = (area.width).min(50);
    let popup_height = (area.height).min(12);
    let popup = centered_rect(popup_width, popup_height, area);
    frame.render_widget(Clear, popup);

    let title = format!(" PR #{} ", pr.number);
    let block = Block::default()
        .title(Span::styled(title, theme::title()))
        .borders(Borders::ALL)
        .border_style(theme::border_active())
        .style(Style::default().bg(theme::COMB));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut y = inner.y + 1;

    // PR title (wrapped)
    let title_area = Rect::new(inner.x + 1, y, inner.width.saturating_sub(2), 2);
    frame.render_widget(
        Paragraph::new(pr.title.as_str())
            .style(theme::text())
            .wrap(Wrap { trim: true }),
        title_area,
    );
    y += 2;

    // State badge
    let state_style = match pr.state.as_str() {
        "MERGED" => theme::success(),
        "OPEN" => Style::default().fg(theme::MINT),
        _ => theme::muted(),
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" state  ", theme::muted()),
            Span::styled(pr.state.to_lowercase(), state_style),
        ])),
        Rect::new(inner.x + 1, y, inner.width.saturating_sub(2), 1),
    );
    y += 1;

    // URL (wrapped)
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" url    ", theme::muted()),
            Span::styled(&pr.url, theme::accent()),
        ]))
        .wrap(Wrap { trim: false }),
        Rect::new(inner.x + 1, y, inner.width.saturating_sub(2), 2),
    );

    // Hints at bottom
    let hint = Line::from(vec![
        Span::styled("o", theme::key_hint()),
        Span::styled(" open  ", theme::key_desc()),
        Span::styled("c", theme::key_hint()),
        Span::styled(" copy  ", theme::key_desc()),
        Span::styled("esc", theme::key_hint()),
        Span::styled(" close", theme::key_desc()),
    ]);
    let hint_area = Rect::new(
        inner.x + 1,
        inner.y + inner.height - 1,
        inner.width.saturating_sub(2),
        1,
    );
    frame.render_widget(Paragraph::new(hint), hint_area);
}

// ── Helpers ────────────────────────────────────────────────

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else if max > 1 {
        format!("{}~", &s[..max - 1])
    } else {
        "~".to_string()
    }
}
