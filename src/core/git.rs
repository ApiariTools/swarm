#![allow(dead_code)]

use color_eyre::{Result, eyre::eyre};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Check if a path is inside a git repo.
pub fn is_git_repo(path: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(path)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Get the repo root for a given path.
pub fn repo_root(path: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(path)
        .output()?;

    if !output.status.success() {
        return Err(eyre!("not a git repo: {}", path.display()));
    }

    let root = String::from_utf8(output.stdout)?.trim().to_string();
    Ok(PathBuf::from(root))
}

/// Get the current branch name.
pub fn current_branch(path: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(path)
        .output()?;

    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

/// Get the short SHA of HEAD.
pub fn head_short_sha(path: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(path)
        .output()?;

    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

/// Get the repo name from the directory.
pub fn repo_name(path: &Path) -> String {
    repo_root(path)
        .ok()
        .and_then(|r| r.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "unknown".to_string())
}

/// Check if a branch exists.
pub fn branch_exists(repo_path: &Path, branch: &str) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", branch])
        .current_dir(repo_path)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Fetch from origin remote.
/// Returns Ok(true) if fetch succeeded, Ok(false) if no remote or fetch failed.
pub fn fetch_origin(repo_path: &Path) -> Result<bool> {
    let output = Command::new("git")
        .args(["fetch", "origin"])
        .current_dir(repo_path)
        .output()?;
    Ok(output.status.success())
}

/// Count how many commits `local` is behind `remote`.
pub fn commits_behind(repo_path: &Path, local: &str, remote: &str) -> Result<usize> {
    let range = format!("{}..{}", local, remote);
    let output = Command::new("git")
        .args(["rev-list", "--count", &range])
        .current_dir(repo_path)
        .output()?;
    let text = String::from_utf8(output.stdout)?.trim().to_string();
    Ok(text.parse().unwrap_or(0))
}

/// Try to fast-forward merge the current branch to a remote ref.
/// Returns Ok(true) if ff-only merge succeeded, Ok(false) if not possible.
pub fn merge_ff_only(repo_path: &Path, remote_ref: &str) -> Result<bool> {
    let output = Command::new("git")
        .args(["merge", "--ff-only", remote_ref])
        .current_dir(repo_path)
        .output()?;
    Ok(output.status.success())
}

/// Create a worktree with a new branch. If the branch already exists,
/// reuse it (checkout existing branch into worktree).
/// If `start_point` is provided, the new branch is created from that ref
/// (e.g. "origin/main") instead of HEAD.
pub fn create_worktree(
    repo_path: &Path,
    branch: &str,
    worktree_path: &Path,
    start_point: Option<&str>,
) -> Result<()> {
    let args = if branch_exists(repo_path, branch) {
        // Branch exists — use it without -b
        vec![
            "worktree".to_string(),
            "add".to_string(),
            worktree_path.to_string_lossy().to_string(),
            branch.to_string(),
        ]
    } else {
        // New branch
        let mut v = vec![
            "worktree".to_string(),
            "add".to_string(),
            "-b".to_string(),
            branch.to_string(),
            worktree_path.to_string_lossy().to_string(),
        ];
        if let Some(sp) = start_point {
            v.push(sp.to_string());
        }
        v
    };

    let output = Command::new("git")
        .args(&args)
        .current_dir(repo_path)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!("failed to create worktree: {}", stderr));
    }

    Ok(())
}

/// Remove a worktree.
pub fn remove_worktree(repo_path: &Path, worktree_path: &Path) -> Result<()> {
    let output = Command::new("git")
        .args([
            "worktree",
            "remove",
            "--force",
            &worktree_path.to_string_lossy(),
        ])
        .current_dir(repo_path)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!("failed to remove worktree: {}", stderr));
    }

    Ok(())
}

/// Delete a branch.
pub fn delete_branch(repo_path: &Path, branch: &str) -> Result<()> {
    Command::new("git")
        .args(["branch", "-D", branch])
        .current_dir(repo_path)
        .output()?;
    Ok(())
}

/// Prune stale worktree entries (directories that no longer exist).
pub fn prune_worktrees(repo_path: &Path) -> Result<()> {
    Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(repo_path)
        .output()?;
    Ok(())
}

/// Check if a branch is currently checked out in any worktree.
pub fn branch_in_worktree(repo_path: &Path, branch: &str) -> bool {
    list_worktrees(repo_path)
        .unwrap_or_default()
        .iter()
        .any(|(_, b)| b == branch)
}

/// List worktrees for a repo, returns (path, branch) pairs.
pub fn list_worktrees(repo_path: &Path) -> Result<Vec<(PathBuf, String)>> {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_path)
        .output()?;

    let text = String::from_utf8(output.stdout)?;
    let mut worktrees = Vec::new();
    let mut current_path: Option<PathBuf> = None;

    for line in text.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current_path = Some(PathBuf::from(path));
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
            if let Some(path) = current_path.take() {
                worktrees.push((path, branch.to_string()));
            }
        } else if line.is_empty() {
            current_path = None;
        }
    }

    Ok(worktrees)
}

/// Detect git repos in a directory (for multi-repo workspaces).
/// Scans immediate children first — if any are git repos, use those.
/// Falls back to the directory itself if it's a repo with no sub-repos.
pub fn detect_repos(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut child_repos = Vec::new();

    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir()
                && !path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
                && is_git_repo(&path)
            {
                child_repos.push(path);
            }
        }
    }

    if !child_repos.is_empty() {
        // Sort by recent commit count (most active first).
        // Compute counts once upfront instead of spawning git per comparison.
        let mut counted: Vec<(PathBuf, usize)> = child_repos
            .into_iter()
            .map(|repo| {
                let c = std::process::Command::new("git")
                    .args(["rev-list", "--count", "--since=3 months ago", "HEAD"])
                    .current_dir(&repo)
                    .output()
                    .ok()
                    .and_then(|o| {
                        String::from_utf8_lossy(&o.stdout)
                            .trim()
                            .parse()
                            .ok()
                    })
                    .unwrap_or(0);
                (repo, c)
            })
            .collect();
        counted.sort_by(|a, b| b.1.cmp(&a.1));
        child_repos = counted.into_iter().map(|(p, _)| p).collect();
        return Ok(child_repos);
    }

    // No child repos — use dir itself if it's a repo
    let mut repos = Vec::new();
    if is_git_repo(dir) {
        repos.push(dir.to_path_buf());
    }
    Ok(repos)
}

/// Generate a `swarm/<sanitized-prompt>-<suffix>` branch name.
pub fn generate_branch_name(prompt: &str, suffix: &str) -> String {
    format!("swarm/{}-{}", super::shell::sanitize(prompt), suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── generate_branch_name tests ───────────────────────────

    #[test]
    fn test_branch_name_sanitizes_spaces() {
        let name = generate_branch_name("fix the login bug", "a1b2");
        assert_eq!(name, "swarm/fix-the-login-bug-a1b2");
    }

    #[test]
    fn test_branch_name_truncates_long_prompts() {
        let long_prompt = "a".repeat(60);
        let name = generate_branch_name(&long_prompt, "x1y2");
        // sanitize truncates to 40 chars, so the prompt part is 40 chars max
        let after_prefix = name.strip_prefix("swarm/").unwrap();
        // after_prefix = "<sanitized>-x1y2"
        let parts: Vec<&str> = after_prefix.rsplitn(2, '-').collect();
        assert_eq!(parts[0], "x1y2");
        // The sanitized prompt portion should be <= 40 chars
        assert!(parts[1].len() <= 40);
    }

    #[test]
    fn test_branch_name_removes_special_chars() {
        let name = generate_branch_name("add user auth (v2) @#$!", "c3d4");
        assert!(!name.contains('('));
        assert!(!name.contains(')'));
        assert!(!name.contains('@'));
        assert!(!name.contains('#'));
        assert!(!name.contains('$'));
        assert!(!name.contains('!'));
        assert!(name.starts_with("swarm/"));
        assert!(name.ends_with("-c3d4"));
    }

    #[test]
    fn test_branch_name_appends_unique_suffix() {
        let name1 = generate_branch_name("fix bug", "aaaa");
        let name2 = generate_branch_name("fix bug", "bbbb");
        assert!(name1.ends_with("-aaaa"));
        assert!(name2.ends_with("-bbbb"));
        assert_ne!(name1, name2);
    }

    #[test]
    fn test_branch_name_lowercases() {
        let name = generate_branch_name("Fix The BUG", "e5f6");
        assert_eq!(name, "swarm/fix-the-bug-e5f6");
    }

    #[test]
    fn test_branch_name_empty_prompt() {
        let name = generate_branch_name("", "g7h8");
        // sanitize("") == "", so branch is "swarm/-g7h8"
        assert_eq!(name, "swarm/-g7h8");
    }
}
