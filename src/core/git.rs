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

/// Symlink gitignored config files from the repo root into a new worktree.
///
/// Automatically symlinks:
/// - `.env*` files (`.env`, `.env.local`, `.env.development`, etc.)
///
/// If `.swarm/worktree-links` exists in the repo, also symlinks each
/// listed path (one relative path per line).
///
/// Failures are logged but never fatal — a missing `.env` shouldn't
/// prevent the worktree from being created.
pub fn symlink_worktree_files(repo_path: &Path, worktree_path: &Path) -> Vec<PathBuf> {
    let mut linked = Vec::new();

    // Auto-symlink .env* files from repo root
    if let Ok(entries) = std::fs::read_dir(repo_path) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with(".env") && entry.file_type().is_ok_and(|ft| ft.is_file()) {
                let target = worktree_path.join(&name);
                if !target.exists() {
                    if let Err(e) = std::os::unix::fs::symlink(entry.path(), &target) {
                        eprintln!("failed to symlink {}: {e}", name_str);
                    } else {
                        linked.push(PathBuf::from(&*name_str));
                    }
                }
            }
        }
    }

    // Read .swarm/worktree-links manifest if present
    let manifest = repo_path.join(".swarm").join("worktree-links");
    if let Ok(contents) = std::fs::read_to_string(&manifest) {
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let src = repo_path.join(line);
            let dst = worktree_path.join(line);
            if !src.exists() {
                eprintln!("worktree-links: {line} not found in repo, skipping");
                continue;
            }
            if dst.exists() {
                continue;
            }
            // Ensure parent directory exists in worktree
            if let Some(parent) = dst.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::os::unix::fs::symlink(&src, &dst) {
                eprintln!("failed to symlink {line}: {e}");
            } else {
                linked.push(PathBuf::from(line));
            }
        }
    }

    linked
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
/// Recursively walks subdirectories looking for directories that contain a
/// `.git` entry. Once a git repo is found, its subtree is not traversed
/// further (repos are not expected to be nested).
/// If `dir` itself is a git repo, returns it directly without recursing.
pub fn detect_repos(dir: &Path) -> Result<Vec<PathBuf>> {
    // Short-circuit: if dir itself is a repo, return it directly to avoid
    // recursing into potentially large trees (target/, node_modules/, etc.).
    if dir.join(".git").exists() {
        return Ok(vec![dir.to_path_buf()]);
    }

    let mut child_repos = Vec::new();
    find_repos_recursive(dir, &mut child_repos);

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
                    .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
                    .unwrap_or(0);
                (repo, c)
            })
            .collect();
        counted.sort_by_key(|b| std::cmp::Reverse(b.1));
        child_repos = counted.into_iter().map(|(p, _)| p).collect();
    }

    Ok(child_repos)
}

/// Recursively walk `dir` looking for git repos (directories containing a
/// `.git` entry). Skips hidden directories and symlinks. When a repo is found,
/// it is added to `repos` and its subtree is not descended into further.
fn find_repos_recursive(dir: &Path, repos: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Skip symlinks to avoid cycles.
        let is_symlink = entry.file_type().map(|ft| ft.is_symlink()).unwrap_or(false);
        if is_symlink || !path.is_dir() {
            continue;
        }
        if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        {
            continue;
        }
        if path.join(".git").exists() {
            repos.push(path);
        } else {
            find_repos_recursive(&path, repos);
        }
    }
}

/// Ensure the base repo is checked out to `main`.
///
/// Returns `Ok(())` on success. Errors are logged as warnings and also
/// returned so callers can skip `pull_main` when checkout fails (to avoid
/// fast-forwarding an unrelated branch).
pub fn checkout_main(repo_path: &Path) -> Result<()> {
    let output = match Command::new("git")
        .args(["checkout", "main"])
        .current_dir(repo_path)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(repo = %repo_path.display(), error = %e, "checkout_main: failed to spawn git");
            return Err(e.into());
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let err = eyre!("git checkout main failed: {}", stderr);
        tracing::warn!(repo = %repo_path.display(), error = %err, "checkout_main: failed");
        return Err(err);
    }

    Ok(())
}

/// Fast-forward local `main` to `origin/main`.
///
/// Fetches from origin, checks if main is behind, and performs a fast-forward
/// merge if needed. All errors are logged as warnings but never propagated —
/// this is a best-effort operation.
pub fn pull_main(repo_path: &Path) {
    match fetch_origin(repo_path) {
        Ok(false) => {
            tracing::warn!(repo = %repo_path.display(), "pull_main: fetch origin failed");
            return;
        }
        Err(e) => {
            tracing::warn!(repo = %repo_path.display(), error = %e, "pull_main: fetch origin failed");
            return;
        }
        Ok(true) => {}
    }

    let behind = match commits_behind(repo_path, "main", "origin/main") {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, "pull_main: could not check commits behind");
            return;
        }
    };

    if behind == 0 {
        tracing::debug!(repo = %repo_path.display(), "pull_main: already up to date");
        return;
    }

    match merge_ff_only(repo_path, "origin/main") {
        Ok(true) => {
            tracing::info!(repo = %repo_path.display(), commits = behind, "pull_main: fast-forwarded local main");
        }
        Ok(false) => {
            tracing::warn!(repo = %repo_path.display(), "pull_main: fast-forward not possible, local main may have diverged");
        }
        Err(e) => {
            tracing::warn!(repo = %repo_path.display(), error = %e, "pull_main: merge failed");
        }
    }
}

/// Generate a `swarm/<sanitized-prompt>-<suffix>` branch name.
pub fn generate_branch_name(prompt: &str, suffix: &str) -> String {
    format!("swarm/{}-{}", super::shell::sanitize(prompt), suffix)
}

/// Use the `claude` CLI to generate a short, meaningful branch name from the prompt.
///
/// Falls back to the slug-based `generate_branch_name` if the AI call fails or
/// times out (2s).
pub async fn generate_branch_name_ai(prompt: &str, suffix: &str) -> String {
    let prompt_text = prompt.to_string();
    let suffix_owned = suffix.to_string();

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::task::spawn_blocking({
            let prompt_text = prompt_text.clone();
            move || {
                let output = Command::new("claude")
                    .args([
                        "-p",
                        "--model", "haiku",
                        "--no-session-persistence",
                        "--system-prompt",
                        "Generate a short git branch name (max 5 words, kebab-case, no prefix) that summarizes this task. Reply with ONLY the branch name, nothing else.",
                        &prompt_text,
                    ])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .output();

                match output {
                    Ok(o) if o.status.success() => {
                        let raw = String::from_utf8_lossy(&o.stdout).trim().to_string();
                        let cleaned = sanitize_ai_branch_name(&raw);
                        if cleaned.is_empty() {
                            None
                        } else {
                            Some(cleaned)
                        }
                    }
                    _ => None,
                }
            }
        }),
    )
    .await;

    match result {
        Ok(Ok(Some(name))) => {
            tracing::info!(ai_branch = %name, "AI-generated branch name");
            format!("swarm/{}-{}", name, suffix_owned)
        }
        Ok(Ok(None)) => {
            tracing::debug!("AI branch name was empty, falling back to slug");
            generate_branch_name(&prompt_text, &suffix_owned)
        }
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "AI branch name task failed, falling back to slug");
            generate_branch_name(&prompt_text, &suffix_owned)
        }
        Err(_) => {
            tracing::debug!("AI branch name timed out, falling back to slug");
            generate_branch_name(&prompt_text, &suffix_owned)
        }
    }
}

/// Sanitize an AI-generated branch name: ensure kebab-case, strip unwanted
/// characters, and truncate.
fn sanitize_ai_branch_name(raw: &str) -> String {
    // Strip backticks, quotes, and whitespace that LLMs sometimes add
    let stripped = raw
        .trim()
        .trim_matches(|c| c == '`' || c == '\'' || c == '"');
    // Remove any `swarm/` prefix the LLM might add despite instructions
    let stripped = stripped.strip_prefix("swarm/").unwrap_or(stripped);
    super::shell::sanitize(stripped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_branch_name_basic() {
        let name = generate_branch_name("fix the auth bug", "a1b2");
        assert_eq!(name, "swarm/fix-the-auth-bug-a1b2");
    }

    #[test]
    fn sanitize_ai_branch_name_clean() {
        assert_eq!(
            sanitize_ai_branch_name("config-array-support"),
            "config-array-support"
        );
    }

    #[test]
    fn sanitize_ai_branch_name_strips_backticks() {
        assert_eq!(
            sanitize_ai_branch_name("`config-array-support`"),
            "config-array-support"
        );
    }

    #[test]
    fn sanitize_ai_branch_name_strips_quotes() {
        assert_eq!(
            sanitize_ai_branch_name("\"config-array-support\""),
            "config-array-support"
        );
    }

    #[test]
    fn sanitize_ai_branch_name_strips_swarm_prefix() {
        assert_eq!(
            sanitize_ai_branch_name("swarm/config-array-support"),
            "config-array-support"
        );
    }

    #[test]
    fn sanitize_ai_branch_name_trims_whitespace() {
        assert_eq!(
            sanitize_ai_branch_name("  config-array-support  \n"),
            "config-array-support"
        );
    }

    #[test]
    fn sanitize_ai_branch_name_empty_returns_empty() {
        assert_eq!(sanitize_ai_branch_name(""), "");
    }

    #[test]
    fn checkout_main_switches_to_main() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();

        // Init a repo with a commit on main
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(repo)
            .output()
            .unwrap();
        Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@test",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ])
            .current_dir(repo)
            .output()
            .unwrap();
        // Create and switch to another branch
        Command::new("git")
            .args(["checkout", "-b", "other"])
            .current_dir(repo)
            .output()
            .unwrap();

        assert!(checkout_main(repo).is_ok());

        // Verify we're on main
        let out = Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(repo)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "main");
    }

    #[test]
    fn checkout_main_fails_without_main_branch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();

        // Init a repo with a commit on a non-main branch
        Command::new("git")
            .args(["init", "-b", "develop"])
            .current_dir(repo)
            .output()
            .unwrap();
        Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@test",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ])
            .current_dir(repo)
            .output()
            .unwrap();

        assert!(checkout_main(repo).is_err());
    }
}
