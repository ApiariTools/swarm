#![allow(dead_code)]

use color_eyre::{Result, eyre::eyre};
use std::process::Command;

/// Check if we're inside a tmux session.
pub fn inside_tmux() -> bool {
    std::env::var("TMUX").is_ok()
}

/// Check if tmux is installed.
pub fn has_tmux() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Get the current tmux session name.
pub fn current_session() -> Result<String> {
    let output = Command::new("tmux")
        .args(["display-message", "-p", "#{session_name}"])
        .output()?;

    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

/// Create a new tmux session (detached).
pub fn create_session(name: &str, dir: &str) -> Result<()> {
    let output = Command::new("tmux")
        .args(["new-session", "-d", "-s", name, "-c", dir])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!("failed to create session: {}", stderr));
    }

    Ok(())
}

/// Create a new detached tmux session that runs a specific command
/// (instead of a shell). The pane's process IS the command.
pub fn create_session_with_cmd(name: &str, dir: &str, cmd: &str) -> Result<()> {
    let output = Command::new("tmux")
        .args([
            "new-session", "-d", "-s", name, "-c", dir, cmd,
        ])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!("failed to create session: {}", stderr));
    }

    Ok(())
}

/// Create a new window in a session and return the window index.
pub fn create_window(session: &str, name: &str, dir: &str) -> Result<String> {
    let output = Command::new("tmux")
        .args([
            "new-window",
            "-t",
            session,
            "-n",
            name,
            "-c",
            dir,
            "-P",
            "-F",
            "#{window_index}",
        ])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!("failed to create window: {}", stderr));
    }

    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

/// Send literal text to a tmux pane, then press Enter.
/// Uses -l flag so text is never interpreted as tmux key names.
pub fn send_keys(target: &str, text: &str) -> Result<()> {
    // Send text literally (no key-name interpretation)
    Command::new("tmux")
        .args(["send-keys", "-t", target, "-l", text])
        .output()?;

    // Send Enter as a key event
    Command::new("tmux")
        .args(["send-keys", "-t", target, "Enter"])
        .output()?;

    Ok(())
}

/// Send raw keys to a tmux pane (no Enter appended).
/// Use tmux key names: "Enter", "BSpace", "Tab", "C-c", "Up", "Down", etc.
pub fn send_keys_raw(target: &str, keys: &str) -> Result<()> {
    Command::new("tmux")
        .args(["send-keys", "-t", target, keys])
        .output()?;

    Ok(())
}

/// Fire-and-forget: send keys without waiting for tmux to respond.
/// Used in focus mode where latency matters more than error handling.
pub fn send_keys_fire(target: &str, keys: &str) {
    let _ = Command::new("tmux")
        .args(["send-keys", "-t", target, keys])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Kill a tmux window.
pub fn kill_window(target: &str) -> Result<()> {
    Command::new("tmux")
        .args(["kill-window", "-t", target])
        .output()?;

    Ok(())
}

/// Select/focus a tmux window.
pub fn select_window(target: &str) -> Result<()> {
    Command::new("tmux")
        .args(["select-window", "-t", target])
        .output()?;

    Ok(())
}

/// Capture pane content.
pub fn capture_pane(target: &str, lines: u32) -> Result<String> {
    let start = format!("-{}", lines);
    let output = Command::new("tmux")
        .args([
            "capture-pane",
            "-t",
            target,
            "-p",
            "-S",
            &start,
            "-J",
        ])
        .output()?;

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// List windows in a session. Returns vec of (index, name, active).
pub fn list_windows(session: &str) -> Result<Vec<(String, String, bool)>> {
    let output = Command::new("tmux")
        .args([
            "list-windows",
            "-t",
            session,
            "-F",
            "#{window_index}\t#{window_name}\t#{window_active}",
        ])
        .output()?;

    let text = String::from_utf8(output.stdout)?;
    let mut windows = Vec::new();

    for line in text.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 3 {
            windows.push((
                parts[0].to_string(),
                parts[1].to_string(),
                parts[2] == "1",
            ));
        }
    }

    Ok(windows)
}

/// Attach to a tmux session.
pub fn attach_session(name: &str) -> Result<()> {
    let status = Command::new("tmux")
        .args(["attach-session", "-t", name])
        .status()?;

    if !status.success() {
        return Err(eyre!("failed to attach to session: {}", name));
    }

    Ok(())
}

/// Switch the current tmux client to a different session.
/// Use this when already inside tmux instead of attach.
pub fn switch_client(session: &str) -> Result<()> {
    let output = Command::new("tmux")
        .args(["switch-client", "-t", session])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!("failed to switch client: {}", stderr));
    }

    Ok(())
}

/// Switch to a specific window, handling both inside/outside tmux.
/// If inside tmux and target is in a different session, switches session first.
pub fn jump_to(target: &str) -> Result<()> {
    if inside_tmux() {
        // target is "session:window" — extract session name
        let session = target.split(':').next().unwrap_or(target);
        let current = current_session().unwrap_or_default();

        if session != current {
            switch_client(session)?;
        }
        select_window(target)?;
    } else {
        // Not in tmux — attach to the session
        let session = target.split(':').next().unwrap_or(target);
        attach_session(session)?;
    }

    Ok(())
}

/// Check if a session exists.
pub fn session_exists(name: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ── Pane Operations ─────────────────────────────────────────

/// Info about a tmux pane.
#[derive(Debug, Clone)]
pub struct PaneInfo {
    pub pane_id: String,
    pub title: String,
    pub pid: u32,
    pub width: u16,
    pub height: u16,
}

/// Split a pane horizontally (new pane to the right). Returns `%pane_id`.
pub fn split_pane_horizontal(target: &str, dir: &str, percent: u32) -> Result<String> {
    let pct_str = percent.to_string();
    let mut args = vec![
        "split-window",
        "-h",
        "-t",
        target,
        "-c",
        dir,
        "-P",
        "-F",
        "#{pane_id}",
    ];
    if percent > 0 && percent < 100 {
        args.push("-p");
        args.push(&pct_str);
    }
    let output = Command::new("tmux").args(&args).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!("failed to split pane horizontal: {}", stderr));
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

/// Split a pane vertically (new pane below). Returns `%pane_id`.
pub fn split_pane_vertical(target: &str, dir: &str) -> Result<String> {
    let output = Command::new("tmux")
        .args([
            "split-window",
            "-v",
            "-t",
            target,
            "-c",
            dir,
            "-P",
            "-F",
            "#{pane_id}",
        ])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!("failed to split pane vertical: {}", stderr));
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

/// Focus a pane by its ID (e.g. `%3`).
pub fn select_pane(pane_id: &str) -> Result<()> {
    let output = Command::new("tmux")
        .args(["select-pane", "-t", pane_id])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!("failed to select pane: {}", stderr));
    }
    Ok(())
}

/// Kill a pane by its ID.
pub fn kill_pane(pane_id: &str) -> Result<()> {
    Command::new("tmux")
        .args(["kill-pane", "-t", pane_id])
        .output()?;
    Ok(())
}

/// Set a pane's border title.
pub fn set_pane_title(pane_id: &str, title: &str) -> Result<()> {
    Command::new("tmux")
        .args(["select-pane", "-t", pane_id, "-T", title])
        .output()?;
    Ok(())
}

/// List panes in a target (session:window). Returns pane info.
pub fn list_panes(target: &str) -> Result<Vec<PaneInfo>> {
    let output = Command::new("tmux")
        .args([
            "list-panes",
            "-t",
            target,
            "-F",
            "#{pane_id}\t#{pane_title}\t#{pane_pid}\t#{pane_width}\t#{pane_height}",
        ])
        .output()?;
    let text = String::from_utf8(output.stdout)?;
    let mut panes = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 5 {
            panes.push(PaneInfo {
                pane_id: parts[0].to_string(),
                title: parts[1].to_string(),
                pid: parts[2].parse().unwrap_or(0),
                width: parts[3].parse().unwrap_or(0),
                height: parts[4].parse().unwrap_or(0),
            });
        }
    }
    Ok(panes)
}

/// Check if a pane exists by ID.
pub fn pane_exists(pane_id: &str) -> bool {
    Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}",
        ])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.trim() == pane_id)
        })
        .unwrap_or(false)
}

/// Apply session styling: pane borders, colors, background, disable status bar.
pub fn apply_session_style(session: &str) -> Result<()> {
    // Dark background + light text for ALL panes (COMB bg, FROST fg)
    let _ = Command::new("tmux")
        .args([
            "set-option", "-t", session,
            "window-style", "bg=#282520,fg=#dcdce1",
        ])
        .output();
    let _ = Command::new("tmux")
        .args([
            "set-option", "-t", session,
            "window-active-style", "bg=#282520,fg=#dcdce1",
        ])
        .output();
    // Enable pane border status (shows titles)
    let _ = Command::new("tmux")
        .args(["set-option", "-t", session, "pane-border-status", "top"])
        .output();
    // Set pane border format: selection-aware conditional
    // Selected panes get a full-width colored line; non-selected get a dimmed small swatch
    let border_fmt = concat!(
        "#{?#{@selected},",
        "#[fg=#{@color}]\u{2501}\u{2501} #{pane_title} ",
        "\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}",
        "\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}",
        "\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}",
        "\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}",
        "\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}",
        "\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}",
        "\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}",
        "\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}",
        "\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}",
        "\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}",
        "\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}",
        "\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}",
        "\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}",
        "\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}",
        "\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}",
        "\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}",
        "#[default]",
        ",",
        " #[fg=#{@color}]\u{2588}\u{2588}#[default]#[fg=#5a5550] #{pane_title} #[default]}",
    );
    let _ = Command::new("tmux")
        .args([
            "set-option", "-t", session,
            "pane-border-format",
            border_fmt,
        ])
        .output();
    // Inactive pane border color (WAX gray)
    let _ = Command::new("tmux")
        .args([
            "set-option", "-t", session,
            "pane-border-style", "fg=#3c3830,bg=#282520",
        ])
        .output();
    // Active pane border color (HONEY amber)
    let _ = Command::new("tmux")
        .args([
            "set-option", "-t", session,
            "pane-active-border-style", "fg=#ffb74d,bg=#282520",
        ])
        .output();
    // Disable the tmux status bar
    let _ = Command::new("tmux")
        .args(["set-option", "-t", session, "status", "off"])
        .output();
    Ok(())
}

/// Set a pane's individual style (background/foreground).
/// Uses set-option instead of select-pane to avoid focusing the pane as a side effect.
pub fn set_pane_style(pane_id: &str, style: &str) -> Result<()> {
    Command::new("tmux")
        .args(["set-option", "-p", "-t", pane_id, "style", style])
        .output()?;
    Ok(())
}

/// Set a pane's @color option (used by pane-border-format for colored titles).
pub fn set_pane_color(pane_id: &str, hex_color: &str) -> Result<()> {
    Command::new("tmux")
        .args([
            "set-option", "-p", "-t", pane_id,
            "@color", hex_color,
        ])
        .output()?;
    Ok(())
}

/// Set a pane's @selected user option (used by border format conditional).
pub fn set_pane_selected(pane_id: &str, selected: bool) -> Result<()> {
    Command::new("tmux")
        .args(["set-option", "-p", "-t", pane_id,
               "@selected", if selected { "1" } else { "0" }])
        .output()?;
    Ok(())
}

/// Apply main-vertical layout with fixed sidebar width.
pub fn apply_layout(session_window: &str, sidebar_width: u16) -> Result<()> {
    // Set main pane width for main-vertical layout
    let _ = Command::new("tmux")
        .args([
            "set-option",
            "-t",
            session_window,
            "main-pane-width",
            &sidebar_width.to_string(),
        ])
        .output();
    // Apply the main-vertical layout
    let output = Command::new("tmux")
        .args(["select-layout", "-t", session_window, "main-vertical"])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!("failed to apply layout: {}", stderr));
    }
    Ok(())
}

/// Launch a tmux popup overlay. Fire-and-forget — returns immediately.
pub fn display_popup(target: &str, width: &str, height: &str, title: &str, cmd: &str) -> Result<()> {
    let child = Command::new("tmux")
        .args([
            "display-popup",
            "-E",
            "-t", target,
            "-w", width,
            "-h", height,
            "-T", title,
            "-s", "bg=#282520",
            "-S", "fg=#ffb74d,bg=#282520",
            cmd,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    match child {
        Ok(_) => Ok(()),
        Err(e) => Err(eyre!("failed to open popup: {}", e)),
    }
}

/// Send literal text + Enter to a pane by `%id`.
pub fn send_keys_to_pane(pane_id: &str, text: &str) -> Result<()> {
    Command::new("tmux")
        .args(["send-keys", "-t", pane_id, "-l", text])
        .output()?;
    Command::new("tmux")
        .args(["send-keys", "-t", pane_id, "Enter"])
        .output()?;
    Ok(())
}
