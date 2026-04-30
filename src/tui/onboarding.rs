use crate::core::git;
use crate::tui::theme;

use color_eyre::Result;
use crossterm::ExecutableCommand;
use crossterm::event::{EventStream, KeyCode, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};
use std::io::stdout;
use std::path::{Path, PathBuf};

/// Available default agents.
const AGENTS: &[&str] = &["claude", "codex", "gemini"];

/// Check whether onboarding has already been completed for this workspace.
pub fn needs_onboarding(work_dir: &Path) -> bool {
    !work_dir.join(".swarm").join("onboarded").exists()
}

/// Mark onboarding as complete by writing the marker file.
fn mark_onboarded(work_dir: &Path) -> Result<()> {
    let swarm_dir = work_dir.join(".swarm");
    std::fs::create_dir_all(&swarm_dir)?;
    std::fs::write(swarm_dir.join("onboarded"), "")?;
    Ok(())
}

/// Save the chosen default agent to `.swarm/config.toml`.
fn save_default_agent(work_dir: &Path, agent: &str) -> Result<()> {
    let swarm_dir = work_dir.join(".swarm");
    std::fs::create_dir_all(&swarm_dir)?;
    let config_path = swarm_dir.join("config.toml");
    let content = format!("default_agent = \"{}\"\n", agent);
    std::fs::write(config_path, content)?;
    Ok(())
}

/// Result of the onboarding screen.
pub enum OnboardingResult {
    /// User pressed Enter — launch the TUI.
    Launch,
    /// User pressed q — quit without launching.
    Quit,
}

/// Show the onboarding screen. Blocks until user presses Enter or q.
pub async fn show(work_dir: &Path, repos: &[PathBuf]) -> Result<OnboardingResult> {
    stdout().execute(EnterAlternateScreen)?;
    enable_raw_mode()?;

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    terminal.clear()?;

    let mut agent_index: usize = 0;
    let mut event_stream = EventStream::new();

    // Initial draw
    terminal.draw(|frame| draw(frame, work_dir, repos, agent_index))?;

    let result = loop {
        if let Some(Ok(crossterm::event::Event::Key(key))) = event_stream.next().await {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                break OnboardingResult::Quit;
            }
            match key.code {
                KeyCode::Enter => {
                    let agent = AGENTS[agent_index];
                    save_default_agent(work_dir, agent).ok();
                    mark_onboarded(work_dir)?;
                    break OnboardingResult::Launch;
                }
                KeyCode::Char('q') | KeyCode::Esc => {
                    break OnboardingResult::Quit;
                }
                KeyCode::Left | KeyCode::Char('h') => {
                    agent_index = agent_index.saturating_sub(1);
                }
                KeyCode::Right | KeyCode::Char('l') if agent_index + 1 < AGENTS.len() => {
                    agent_index += 1;
                }
                KeyCode::Char(' ') | KeyCode::Tab => {
                    agent_index = (agent_index + 1) % AGENTS.len();
                }
                _ => {}
            }
            terminal.draw(|frame| draw(frame, work_dir, repos, agent_index))?;
        }
    };

    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;

    Ok(result)
}

/// Render the onboarding screen.
fn draw(frame: &mut Frame, work_dir: &Path, repos: &[PathBuf], agent_index: usize) {
    let area = frame.area();

    // Background
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::COMB)),
        area,
    );

    // Center a box
    let box_width = 58u16.min(area.width.saturating_sub(4));
    let box_height = 22u16.min(area.height.saturating_sub(2));
    let box_area = centered_rect(box_width, box_height, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::HONEY))
        .padding(Padding::new(2, 2, 1, 1))
        .style(Style::default().bg(theme::COMB));

    let inner = block.inner(box_area);
    frame.render_widget(block, box_area);

    // Build content lines
    let mut lines: Vec<Line<'_>> = vec![
        // Title
        Line::from(vec![Span::styled(
            "  Welcome to Swarm",
            Style::default()
                .fg(theme::HONEY)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        // Description
        Line::from(Span::styled(
            "Swarm runs AI coding agents in parallel worktrees.",
            theme::text(),
        )),
        Line::from(Span::styled(
            "Each agent gets its own git branch and works on a",
            theme::text(),
        )),
        Line::from(Span::styled(
            "task independently \u{2014} no conflicts, no waiting.",
            theme::text(),
        )),
        Line::from(""),
        // How it works
        Line::from(Span::styled(
            "How it works:",
            Style::default()
                .fg(theme::FROST)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "1. Dispatch a task \u{2192} agent creates a worktree",
            theme::muted(),
        )),
        Line::from(Span::styled(
            "2. Agent writes code, commits, opens a PR",
            theme::muted(),
        )),
        Line::from(Span::styled(
            "3. You review and merge when ready",
            theme::muted(),
        )),
        Line::from(""),
    ];

    // Detected workspace
    let dir_name = work_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| work_dir.to_string_lossy().to_string());

    lines.push(Line::from(vec![
        Span::styled("Detected workspace: ", theme::muted()),
        Span::styled(
            &dir_name,
            Style::default()
                .fg(theme::FROST)
                .add_modifier(Modifier::BOLD),
        ),
    ]));

    // Repos found
    if !repos.is_empty() {
        let repo_names: Vec<String> = repos.iter().map(|r| git::repo_name(r)).collect();
        let display = if repo_names.len() <= 4 {
            repo_names.join(", ")
        } else {
            let first_three = repo_names[..3].join(", ");
            format!("{}, ... ({} total)", first_three, repo_names.len())
        };
        lines.push(Line::from(vec![
            Span::styled("Repos found: ", theme::muted()),
            Span::styled(display, Style::default().fg(theme::MINT)),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            "Repos found: (scanning...)",
            theme::muted(),
        )));
    }

    lines.push(Line::from(""));

    // Agent selector
    let agent_spans: Vec<Span<'_>> = AGENTS
        .iter()
        .enumerate()
        .flat_map(|(i, &name)| {
            let is_selected = i == agent_index;
            let mut spans = Vec::new();
            if i > 0 {
                spans.push(Span::styled("  ", theme::muted()));
            }
            if is_selected {
                spans.push(Span::styled(
                    format!(" {} ", name),
                    Style::default()
                        .fg(theme::COMB)
                        .bg(theme::HONEY)
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::styled(
                    format!(" {} ", name),
                    Style::default().fg(theme::SMOKE),
                ));
            }
            spans
        })
        .collect();

    let mut agent_line = vec![Span::styled("Default agent  ", theme::muted())];
    agent_line.extend(agent_spans);
    lines.push(Line::from(agent_line));

    lines.push(Line::from(""));

    // Divider
    let div_width = inner.width.saturating_sub(2) as usize;
    lines.push(Line::from(Span::styled(
        "\u{2500}".repeat(div_width),
        Style::default().fg(theme::WAX),
    )));

    // Key hints
    lines.push(Line::from(vec![
        Span::styled("Enter", theme::key_hint()),
        Span::styled(" Launch   ", theme::key_desc()),
        Span::styled("\u{2190}/\u{2192}", theme::key_hint()),
        Span::styled(" Agent   ", theme::key_desc()),
        Span::styled("q", theme::key_hint()),
        Span::styled(" Quit", theme::key_desc()),
    ]));

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, inner);
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn needs_onboarding_no_swarm_dir() {
        let tmp = TempDir::new().unwrap();
        assert!(needs_onboarding(tmp.path()));
    }

    #[test]
    fn needs_onboarding_after_mark() {
        let tmp = TempDir::new().unwrap();
        assert!(needs_onboarding(tmp.path()));
        mark_onboarded(tmp.path()).unwrap();
        assert!(!needs_onboarding(tmp.path()));
    }

    #[test]
    fn save_and_read_default_agent() {
        let tmp = TempDir::new().unwrap();
        save_default_agent(tmp.path(), "codex").unwrap();
        let content = std::fs::read_to_string(tmp.path().join(".swarm/config.toml")).unwrap();
        assert!(content.contains("default_agent = \"codex\""));
    }
}
