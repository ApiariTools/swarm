#![allow(dead_code)]

use color_eyre::{Result, eyre::eyre};
use std::collections::HashMap;
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

/// Set a pane's border title and lock it so child processes cannot override it.
pub fn set_pane_title(pane_id: &str, title: &str) -> Result<()> {
    Command::new("tmux")
        .args(["select-pane", "-t", pane_id, "-T", title])
        .output()?;
    // Prevent the child process (e.g. Claude Code) from overriding the title
    // via terminal escape sequences.
    Command::new("tmux")
        .args(["set-option", "-p", "-t", pane_id, "allow-rename", "off"])
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
    // Default pane style: dimmed (non-selected panes recede visually).
    // Per-pane overrides brighten the selected worktree's panes.
    let _ = Command::new("tmux")
        .args([
            "set-option", "-t", session,
            "window-style", "bg=#141210,fg=#3a3835,dim",
        ])
        .output();
    // Active pane (sidebar, or whichever has tmux focus) stays bright
    let _ = Command::new("tmux")
        .args([
            "set-option", "-t", session,
            "window-active-style", "bg=#302c26,fg=#dcdce1,nodim",
        ])
        .output();
    // Padded borders for visible gaps between panes
    let _ = Command::new("tmux")
        .args(["set-option", "-t", session, "pane-border-lines", "padded"])
        .output();
    // Enable pane border status (shows titles)
    let _ = Command::new("tmux")
        .args(["set-option", "-t", session, "pane-border-status", "top"])
        .output();
    // Set pane border format: selection-aware conditional
    // Sidebar panes (@sidebar=1) get no border title at all.
    // Selected panes get a full-width colored line; non-selected get a dimmed small swatch.
    let border_fmt = concat!(
        "#{?#{@sidebar},,",
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
        "}",
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

/// Get the window dimensions for a session:window target.
pub fn get_window_size(session_window: &str) -> Result<(u16, u16)> {
    let output = Command::new("tmux")
        .args([
            "display-message",
            "-t",
            session_window,
            "-p",
            "#{window_width} #{window_height}",
        ])
        .output()?;
    let text = String::from_utf8(output.stdout)?.trim().to_string();
    let parts: Vec<&str> = text.split_whitespace().collect();
    if parts.len() >= 2 {
        let w: u16 = parts[0].parse().unwrap_or(200);
        let h: u16 = parts[1].parse().unwrap_or(50);
        Ok((w, h))
    } else {
        Err(eyre!("failed to parse window size"))
    }
}

/// Get mapping of pane_id (%N) to pane_index (0-based) for a window.
fn get_pane_indices(target: &str) -> Result<HashMap<String, u16>> {
    let output = Command::new("tmux")
        .args([
            "list-panes",
            "-t",
            target,
            "-F",
            "#{pane_id}\t#{pane_index}",
        ])
        .output()?;
    let text = String::from_utf8(output.stdout)?;
    let mut map = HashMap::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 2 {
            if let Ok(idx) = parts[1].parse::<u16>() {
                map.insert(parts[0].to_string(), idx);
            }
        }
    }
    Ok(map)
}

/// Compute tmux layout checksum (16-bit rotate-and-add from tmux source).
fn layout_checksum(layout: &str) -> u16 {
    let mut csum: u16 = 0;
    for byte in layout.bytes() {
        csum = (csum >> 1) | ((csum & 1) << 15);
        csum = csum.wrapping_add(byte as u16);
    }
    csum
}

/// Build a layout node for vertically stacked panes.
fn build_vertical_stack(panes: &[u16], width: u16, height: u16, x: u16, y: u16) -> String {
    if panes.len() == 1 {
        return format!("{}x{},{},{},{}", width, height, x, y, panes[0]);
    }
    let n = panes.len() as u16;
    let seps = n - 1;
    let usable = height.saturating_sub(seps);
    let base_h = usable / n;
    let remainder = usable % n;

    let mut nodes = Vec::new();
    let mut cy = y;
    for (i, &idx) in panes.iter().enumerate() {
        let h = if (i as u16) == n - 1 {
            (y + height) - cy
        } else {
            base_h + if (i as u16) < remainder { 1 } else { 0 }
        };
        nodes.push(format!("{}x{},{},{},{}", width, h, x, cy, idx));
        cy += h + 1;
    }
    format!("{}x{},{},{}[{}]", width, height, x, y, nodes.join(","))
}

/// Build the right-side area of the layout (one or more columns).
fn build_right_area(columns: &[Vec<u16>], total_w: u16, height: u16, start_x: u16) -> String {
    if columns.len() == 1 {
        return build_vertical_stack(&columns[0], total_w, height, start_x, 0);
    }

    let nc = columns.len() as u16;
    let seps = nc - 1;
    let usable = total_w.saturating_sub(seps);
    let base_w = usable / nc;
    let remainder = usable % nc;

    let mut nodes = Vec::new();
    let mut cx = start_x;
    for (i, col) in columns.iter().enumerate() {
        let w = if (i as u16) == nc - 1 {
            (start_x + total_w) - cx
        } else {
            base_w + if (i as u16) < remainder { 1 } else { 0 }
        };
        nodes.push(build_vertical_stack(col, w, height, cx, 0));
        cx += w + 1;
    }

    format!(
        "{}x{},{},{}{{{}}}",
        total_w,
        height,
        start_x,
        0,
        nodes.join(",")
    )
}

/// Apply a tiled grid layout: sidebar on the left, agent panes in a grid on the right.
/// Falls back to main-vertical if the custom layout fails.
pub fn apply_tiled_layout(
    session_window: &str,
    sidebar_pane_id: &str,
    sidebar_width: u16,
    pane_groups: Vec<Vec<String>>,
) -> Result<()> {
    let pane_map = get_pane_indices(session_window)?;

    // Convert pane IDs to indices, drop empty groups
    let valid_groups: Vec<Vec<u16>> = pane_groups
        .iter()
        .filter_map(|group| {
            let indices: Vec<u16> = group
                .iter()
                .filter_map(|id| pane_map.get(id).copied())
                .collect();
            if indices.is_empty() {
                None
            } else {
                Some(indices)
            }
        })
        .collect();

    if valid_groups.is_empty() {
        return apply_layout(session_window, sidebar_width);
    }

    let (win_w, win_h) = get_window_size(session_window)?;
    let sidebar_idx = pane_map.get(sidebar_pane_id).copied().unwrap_or(0);

    // Determine column count based on number of worktrees with live panes
    let n = valid_groups.len();
    let num_cols = if n <= 1 { 1 } else if n <= 4 { 2 } else { n.min(3) };

    // Distribute worktree groups round-robin across columns
    let mut columns: Vec<Vec<u16>> = vec![vec![]; num_cols];
    for (i, group) in valid_groups.iter().enumerate() {
        columns[i % num_cols].extend(group);
    }
    columns.retain(|c| !c.is_empty());
    if columns.is_empty() {
        return apply_layout(session_window, sidebar_width);
    }

    // Build layout string
    let right_w = win_w.saturating_sub(sidebar_width + 1);
    let right_x = sidebar_width + 1;
    let sidebar_leaf = format!(
        "{}x{},{},{},{}",
        sidebar_width, win_h, 0, 0, sidebar_idx
    );
    let right_node = build_right_area(&columns, right_w, win_h, right_x);

    let body = format!(
        "{}x{},{},{}{{{},{}}}",
        win_w, win_h, 0, 0, sidebar_leaf, right_node
    );
    let csum = layout_checksum(&body);
    let layout = format!("{:04x},{}", csum, body);

    let output = Command::new("tmux")
        .args(["select-layout", "-t", session_window, &layout])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!("select-layout failed: {}", stderr));
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
