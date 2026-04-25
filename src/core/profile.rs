use std::path::Path;

/// Embedded default profile (shipped with the binary).
pub const DEFAULT_PROFILE: &str = include_str!("../../profiles/default.md");

/// Embedded reviewer profile for read-only branch review workers.
pub const REVIEWER_PROFILE: &str = "# Reviewer Profile

## Rules
1. You are a READ-ONLY code reviewer. Do NOT make any code changes, commits, or pull requests.
2. Your only job is to review the branch diff and output a structured verdict.
3. Do not modify any files. Do not run `git commit`, `git push`, or `gh pr create`.
4. Review the diff completely before outputting your verdict.

## How to Review
You will be given a branch name. Run `git diff main...<branch-name>` to see the changes.
Review the diff output and evaluate the changes before producing your verdict.

## Review Focus
Evaluate the changes on these dimensions:
- **Correctness**: Does the code do what it claims? Are there logic errors or off-by-one errors?
- **Safety**: Are there security vulnerabilities, panics, or unsafe operations introduced?
- **API Consistency**: Does the change follow existing patterns and conventions in the codebase?
- **Test Coverage**: Are there tests for new behavior? Do tests cover edge cases?
- **Backward Compatibility**: Does the change break existing interfaces, serialization formats, or behavior?

## Verdict Format
Output EXACTLY one of these as your final message (with no surrounding text on those lines):

If the branch looks good:
```
REVIEW_VERDICT: APPROVED
```

If changes are needed:
```
REVIEW_VERDICT: CHANGES_REQUESTED
- [file:line] description of issue
- [file:line] description of issue
```

## Output
Write your full review (verdict + reasoning) to `.swarm/output.md` in the worktree root. This is read by the coordinator. Keep it concise.
";

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

/// Build the effective prompt by prepending the worker profile to the user's prompt.
///
/// This is the agent-agnostic way to inject profile content: it goes through the
/// prompt rather than convention files, so it works for Claude, Codex, and Gemini.
pub fn build_effective_prompt(profile: &str, user_prompt: &str) -> String {
    format!("{profile}\n\n---\n\n{user_prompt}")
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
    fn reviewer_profile_contains_key_instructions() {
        assert!(REVIEWER_PROFILE.contains("READ-ONLY"));
        assert!(REVIEWER_PROFILE.contains("Do NOT make any code changes"));
        assert!(REVIEWER_PROFILE.contains("REVIEW_VERDICT: APPROVED"));
        assert!(REVIEWER_PROFILE.contains("REVIEW_VERDICT: CHANGES_REQUESTED"));
        assert!(REVIEWER_PROFILE.contains("Correctness"));
        assert!(REVIEWER_PROFILE.contains("Safety"));
    }

    #[test]
    fn build_effective_prompt_prepends_profile() {
        let result = build_effective_prompt("# Profile", "Fix the bug");
        assert_eq!(result, "# Profile\n\n---\n\nFix the bug");
    }

    #[test]
    fn build_effective_prompt_preserves_user_prompt() {
        let result = build_effective_prompt("# Profile", "Fix the bug");
        assert!(result.ends_with("Fix the bug"));
        assert!(result.starts_with("# Profile"));
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
