use crate::core::agent::AgentKind;
use std::path::Path;

/// Embedded default profile (shipped with the binary).
pub const DEFAULT_PROFILE: &str = include_str!("../../profiles/default.md");

/// Load profile by slug from `.swarm/profiles/`. Falls back to embedded default.
pub fn load_profile(work_dir: &Path, slug: &str) -> String {
    let profiles_dir = work_dir.join(".swarm").join("profiles");
    let path = profiles_dir.join(format!("{slug}.md"));
    if path.is_file()
        && let Ok(content) = std::fs::read_to_string(&path)
    {
        return content;
    }
    // Fallback to embedded default for "default" slug
    if slug == "default" {
        return DEFAULT_PROFILE.to_string();
    }
    // Unknown slug with no file — return embedded default with a warning header
    format!("<!-- profile '{slug}' not found, using default -->\n{DEFAULT_PROFILE}")
}

/// Convention filename per agent kind: Claude → "CLAUDE.md", Codex → "AGENTS.md".
pub fn convention_filename(kind: &AgentKind) -> &'static str {
    match kind {
        AgentKind::Claude => "CLAUDE.md",
        AgentKind::Codex => "AGENTS.md",
        AgentKind::Gemini => "GEMINI.md",
    }
}

/// Separator comment used when appending a worker profile to an existing convention file.
const PROFILE_SEPARATOR: &str = "\n\n<!-- swarm worker profile -->\n";

/// Write profile content into the worktree without destroying existing repo instructions.
///
/// - **Claude**: writes to `CLAUDE.local.md` (Claude Code reads both `CLAUDE.md` and
///   `CLAUDE.local.md` automatically, so the repo's shared file is preserved).
/// - **Codex / Gemini**: these agents do not support a `.local.md` override, so the
///   profile is appended to the existing convention file behind a separator comment.
///   If no convention file exists yet, it is created fresh.
pub fn inject_profile(
    worktree_path: &Path,
    kind: &AgentKind,
    content: &str,
) -> std::io::Result<()> {
    match kind {
        AgentKind::Claude => {
            let dest = worktree_path.join("CLAUDE.local.md");
            std::fs::write(&dest, content)
        }
        _ => {
            let filename = convention_filename(kind);
            let dest = worktree_path.join(filename);
            if dest.is_file() {
                let existing = std::fs::read_to_string(&dest)?;
                let merged = format!("{existing}{PROFILE_SEPARATOR}{content}");
                std::fs::write(&dest, merged)
            } else {
                std::fs::write(&dest, content)
            }
        }
    }
}

/// List available profile slugs from `.swarm/profiles/`.
/// Always includes "default" (the embedded fallback).
#[allow(dead_code)]
pub fn list_profiles(work_dir: &Path) -> Vec<String> {
    let mut slugs = vec!["default".to_string()];
    let profiles_dir = work_dir.join(".swarm").join("profiles");
    if let Ok(entries) = std::fs::read_dir(&profiles_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                && stem != "default"
            {
                slugs.push(stem.to_string());
            }
        }
    }
    slugs.sort();
    slugs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn load_profile_fallback_to_default() {
        let tmp = TempDir::new().unwrap();
        let content = load_profile(tmp.path(), "default");
        assert_eq!(content, DEFAULT_PROFILE);
    }

    #[test]
    fn load_profile_custom_file() {
        let tmp = TempDir::new().unwrap();
        let profiles_dir = tmp.path().join(".swarm").join("profiles");
        fs::create_dir_all(&profiles_dir).unwrap();
        fs::write(
            profiles_dir.join("strict.md"),
            "# Strict Profile\nNo fun allowed.",
        )
        .unwrap();

        let content = load_profile(tmp.path(), "strict");
        assert_eq!(content, "# Strict Profile\nNo fun allowed.");
    }

    #[test]
    fn load_profile_unknown_slug_returns_default_with_warning() {
        let tmp = TempDir::new().unwrap();
        let content = load_profile(tmp.path(), "nonexistent");
        assert!(content.contains("profile 'nonexistent' not found"));
        assert!(content.contains("Worker Profile"));
    }

    #[test]
    fn convention_filename_claude() {
        assert_eq!(convention_filename(&AgentKind::Claude), "CLAUDE.md");
    }

    #[test]
    fn convention_filename_codex() {
        assert_eq!(convention_filename(&AgentKind::Codex), "AGENTS.md");
    }

    // --- Claude: writes to CLAUDE.local.md ---

    #[test]
    fn inject_profile_claude_writes_local_md() {
        let tmp = TempDir::new().unwrap();
        inject_profile(tmp.path(), &AgentKind::Claude, "# Test Profile").unwrap();
        let content = fs::read_to_string(tmp.path().join("CLAUDE.local.md")).unwrap();
        assert_eq!(content, "# Test Profile");
    }

    #[test]
    fn inject_profile_claude_does_not_overwrite_shared_file() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("CLAUDE.md"), "# Repo Instructions").unwrap();
        inject_profile(tmp.path(), &AgentKind::Claude, "# Worker Profile").unwrap();
        let shared = fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap();
        assert_eq!(shared, "# Repo Instructions");
        let local = fs::read_to_string(tmp.path().join("CLAUDE.local.md")).unwrap();
        assert_eq!(local, "# Worker Profile");
    }

    // --- Codex: appends to AGENTS.md or creates it ---

    #[test]
    fn inject_profile_codex_creates_agents_md_when_missing() {
        let tmp = TempDir::new().unwrap();
        inject_profile(tmp.path(), &AgentKind::Codex, "# Codex Profile").unwrap();
        let content = fs::read_to_string(tmp.path().join("AGENTS.md")).unwrap();
        assert_eq!(content, "# Codex Profile");
    }

    #[test]
    fn inject_profile_codex_appends_to_existing_agents_md() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("AGENTS.md"), "# Repo Rules").unwrap();
        inject_profile(tmp.path(), &AgentKind::Codex, "# Worker Profile").unwrap();
        let content = fs::read_to_string(tmp.path().join("AGENTS.md")).unwrap();
        assert!(content.starts_with("# Repo Rules"));
        assert!(content.contains("<!-- swarm worker profile -->"));
        assert!(content.ends_with("# Worker Profile"));
    }

    // --- Gemini: appends to GEMINI.md or creates it ---

    #[test]
    fn inject_profile_gemini_creates_gemini_md_when_missing() {
        let tmp = TempDir::new().unwrap();
        inject_profile(tmp.path(), &AgentKind::Gemini, "# Gemini Profile").unwrap();
        let content = fs::read_to_string(tmp.path().join("GEMINI.md")).unwrap();
        assert_eq!(content, "# Gemini Profile");
    }

    #[test]
    fn inject_profile_gemini_appends_to_existing_gemini_md() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("GEMINI.md"), "# Existing Config").unwrap();
        inject_profile(tmp.path(), &AgentKind::Gemini, "# Worker Profile").unwrap();
        let content = fs::read_to_string(tmp.path().join("GEMINI.md")).unwrap();
        assert!(content.starts_with("# Existing Config"));
        assert!(content.contains("<!-- swarm worker profile -->"));
        assert!(content.ends_with("# Worker Profile"));
    }

    #[test]
    fn list_profiles_includes_default() {
        let tmp = TempDir::new().unwrap();
        let slugs = list_profiles(tmp.path());
        assert_eq!(slugs, vec!["default"]);
    }

    #[test]
    fn list_profiles_finds_custom_files() {
        let tmp = TempDir::new().unwrap();
        let profiles_dir = tmp.path().join(".swarm").join("profiles");
        fs::create_dir_all(&profiles_dir).unwrap();
        fs::write(profiles_dir.join("strict.md"), "strict").unwrap();
        fs::write(profiles_dir.join("relaxed.md"), "relaxed").unwrap();
        fs::write(profiles_dir.join("not-a-profile.txt"), "ignored").unwrap();

        let slugs = list_profiles(tmp.path());
        assert_eq!(slugs, vec!["default", "relaxed", "strict"]);
    }

    #[test]
    fn custom_default_profile_overrides_embedded() {
        let tmp = TempDir::new().unwrap();
        let profiles_dir = tmp.path().join(".swarm").join("profiles");
        fs::create_dir_all(&profiles_dir).unwrap();
        fs::write(profiles_dir.join("default.md"), "# Custom Default").unwrap();

        let content = load_profile(tmp.path(), "default");
        assert_eq!(content, "# Custom Default");
    }
}
