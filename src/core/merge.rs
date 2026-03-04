#![allow(dead_code)]

use color_eyre::{Result, eyre::eyre};
use std::path::Path;
use std::process::Command;

/// Stage all changes and commit with a message.
pub fn commit_all(worktree_path: &Path, message: &str) -> Result<()> {
    // Stage everything
    let output = Command::new("git")
        .args(["add", "-A"])
        .current_dir(worktree_path)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!("git add failed: {}", stderr));
    }

    // Check if there's anything to commit
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(worktree_path)
        .output()?;

    if String::from_utf8_lossy(&status.stdout).trim().is_empty() {
        return Ok(()); // Nothing to commit
    }

    // Commit
    let output = Command::new("git")
        .args(["commit", "-m", message])
        .current_dir(worktree_path)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!("git commit failed: {}", stderr));
    }

    Ok(())
}

/// Merge a branch into the base branch.
pub fn merge_into_base(repo_path: &Path, branch: &str, base_branch: &str) -> Result<()> {
    // Checkout base
    let output = Command::new("git")
        .args(["checkout", base_branch])
        .current_dir(repo_path)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!("checkout {} failed: {}", base_branch, stderr));
    }

    // Merge
    let output = Command::new("git")
        .args([
            "merge",
            branch,
            "--no-ff",
            "-m",
            &format!("Merge {}", branch),
        ])
        .current_dir(repo_path)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!("merge failed: {}", stderr));
    }

    Ok(())
}

/// Commit all changes in a worktree and merge the branch into the base branch.
pub fn commit_all_and_merge(repo_path: &Path, worktree_path: &Path, branch: &str) -> Result<()> {
    // Commit any pending changes
    if has_changes(worktree_path)? {
        commit_all(worktree_path, &format!("final commit on {}", branch))?;
    }

    // Determine base branch
    let base = detect_base_branch(repo_path)?;
    merge_into_base(repo_path, branch, &base)
}

/// Detect the default base branch (main or master).
fn detect_base_branch(repo_path: &Path) -> Result<String> {
    for candidate in &["main", "master"] {
        let output = Command::new("git")
            .args(["rev-parse", "--verify", candidate])
            .current_dir(repo_path)
            .output()?;
        if output.status.success() {
            return Ok(candidate.to_string());
        }
    }
    Err(eyre!("could not detect base branch (tried main, master)"))
}

/// Check if a worktree has uncommitted changes.
pub fn has_changes(worktree_path: &Path) -> Result<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(worktree_path)
        .output()?;

    Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

/// Get a short diff stat summary for a worktree.
pub fn diff_stat(worktree_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["diff", "--stat", "HEAD"])
        .current_dir(worktree_path)
        .output()?;

    let stat = String::from_utf8_lossy(&output.stdout).trim().to_string();

    if stat.is_empty() {
        // Check staged
        let output = Command::new("git")
            .args(["diff", "--stat", "--cached"])
            .current_dir(worktree_path)
            .output()?;
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }

    Ok(stat)
}
