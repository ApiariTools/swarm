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

    // Headers are 1 line, worktree rows are 2 lines
    let mut constraints: Vec<Constraint> = items
        .iter()
        .map(|item| match item {
            SidebarItem::RepoHeader(_) => Constraint::Length(1),
            SidebarItem::WorktreeRow(_) => Constraint::Length(2),
        })
        .collect();
    constraints.push(Constraint::Min(0));

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    for (i, item) in items.iter().enumerate() {
        match item {
            SidebarItem::RepoHeader(name) => {
                draw_repo_header(frame, rows[i], name);
            }
            SidebarItem::WorktreeRow(idx) => {
                let wt = &app.worktrees[*idx];
                let is_selected = *idx == app.selected;
                draw_worktree_row(frame, rows[i], wt, is_selected, *idx);
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

/// Dark tinted row backgrounds matching the pane tints.
const ROW_BG_COLORS: &[Color] = &[
    Color::Rgb(48, 36, 24),  // warm brown
    Color::Rgb(24, 32, 48),  // cool blue
    Color::Rgb(24, 48, 24),  // forest green
    Color::Rgb(40, 24, 48),  // purple
    Color::Rgb(24, 48, 48),  // teal
    Color::Rgb(48, 40, 24),  // amber
    Color::Rgb(48, 24, 40),  // rose
    Color::Rgb(32, 48, 24),  // olive
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
    let row_bg = ROW_BG_COLORS[idx % ROW_BG_COLORS.len()];

    let row_style = if selected {
        Style::default().bg(Color::Rgb(58, 50, 42))
    } else {
        Style::default().bg(row_bg)
    };

    // Background
    frame.render_widget(Paragraph::new("").style(row_style), area);

    let row_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
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
    let popup_width = (area.width).min(50);
    let popup = centered_rect(popup_width, 5, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(" task ", theme::title()))
        .borders(Borders::ALL)
        .border_style(theme::border_active())
        .style(Style::default().bg(theme::COMB));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Input line with cursor
    let before: String = app.input_buffer.chars().take(app.input_cursor).collect();
    let cursor_char = app
        .input_buffer
        .chars()
        .nth(app.input_cursor)
        .unwrap_or(' ');
    let after: String = app
        .input_buffer
        .chars()
        .skip(app.input_cursor + 1)
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

    let input_area = Rect::new(inner.x, inner.y + 1, inner.width, 1);
    frame.render_widget(Paragraph::new(input_line), input_area);

    // Hint
    let hint = Line::from(vec![
        Span::styled("enter", theme::key_hint()),
        Span::styled(" submit  ", theme::key_desc()),
        Span::styled("esc", theme::key_hint()),
        Span::styled(" cancel", theme::key_desc()),
    ]);
    let hint_area = Rect::new(inner.x + 1, inner.y + 2, inner.width.saturating_sub(2), 1);
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
