use color_eyre::Result;
use crossterm::{
    ExecutableCommand,
    event::{self, Event, KeyCode, KeyModifiers},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Paragraph, Wrap};
use std::io::stdout;
use std::process::Command;
use std::time::Duration;

use super::theme;

pub struct PrPopupArgs {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub url: String,
}

/// Run the PR detail popup TUI. Designed to run inside a tmux display-popup.
pub fn run_pr_popup(args: PrPopupArgs) -> Result<()> {
    stdout().execute(EnterAlternateScreen)?;
    enable_raw_mode()?;

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    terminal.clear()?;

    let result = pr_popup_loop(&mut terminal, &args);

    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;

    result
}

fn pr_popup_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    args: &PrPopupArgs,
) -> Result<()> {
    loop {
        terminal.draw(|frame| draw_pr_popup(frame, args))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')
                {
                    break;
                }

                match key.code {
                    KeyCode::Char('o') | KeyCode::Enter => {
                        let _ = Command::new("open").arg(&args.url).spawn();
                        break;
                    }
                    KeyCode::Char('c') => {
                        let _ = Command::new("pbcopy")
                            .stdin(std::process::Stdio::piped())
                            .spawn()
                            .and_then(|mut child| {
                                use std::io::Write;
                                if let Some(ref mut stdin) = child.stdin {
                                    stdin.write_all(args.url.as_bytes())?;
                                }
                                child.wait()
                            });
                        break;
                    }
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('p') => break,
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

fn draw_pr_popup(frame: &mut Frame, args: &PrPopupArgs) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::COMB)),
        area,
    );

    let inner = area;
    let mut y = inner.y + 1;

    // PR number header
    let header = Line::from(vec![
        Span::styled(format!(" PR #{}", args.number), theme::title()),
    ]);
    frame.render_widget(
        Paragraph::new(header),
        Rect::new(inner.x, y, inner.width, 1),
    );
    y += 2;

    // PR title (wrapped)
    let title_width = inner.width.saturating_sub(2);
    let title_lines = wrapped_line_count(&args.title, title_width as usize).min(3) as u16;
    frame.render_widget(
        Paragraph::new(args.title.as_str())
            .style(theme::text())
            .wrap(Wrap { trim: true }),
        Rect::new(inner.x + 1, y, title_width, title_lines),
    );
    y += title_lines + 1;

    // State badge
    let state_style = match args.state.as_str() {
        "MERGED" => theme::success(),
        "OPEN" => Style::default().fg(theme::MINT),
        _ => theme::muted(),
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" state  ", theme::muted()),
            Span::styled(args.state.to_lowercase(), state_style),
        ])),
        Rect::new(inner.x, y, inner.width.saturating_sub(1), 1),
    );
    y += 2;

    // URL (wrapped — the whole point of this popup)
    let url_width = inner.width.saturating_sub(2);
    let url_lines = wrapped_line_count(&args.url, url_width as usize).min(4) as u16;
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(&args.url, theme::accent()),
        ]))
        .wrap(Wrap { trim: false }),
        Rect::new(inner.x + 1, y, url_width, url_lines),
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
    let hint_y = inner.y + inner.height.saturating_sub(1);
    frame.render_widget(
        Paragraph::new(hint),
        Rect::new(inner.x + 1, hint_y, inner.width.saturating_sub(2), 1),
    );
}

/// Estimate how many lines a string will occupy when wrapped to a given width.
fn wrapped_line_count(s: &str, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    let len = s.len();
    if len == 0 {
        return 1;
    }
    (len + width - 1) / width
}
