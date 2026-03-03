use crate::core::agent::AgentKind;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Serde helper: deserialize either a single `ReviewConfig` (old format) or
/// `Vec<ReviewConfig>` (new format) into `Option<Vec<ReviewConfig>>`.
pub fn deserialize_review_configs<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<ReviewConfig>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        Many(Vec<ReviewConfig>),
        One(ReviewConfig),
    }

    let opt: Option<OneOrMany> = Option::deserialize(deserializer)?;
    Ok(opt.map(|v| match v {
        OneOrMany::Many(cs) => cs,
        OneOrMany::One(c) => vec![c],
    }))
}

/// Review mode: read-only findings vs. act (modify code).
#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewMode {
    /// Read-only: agent writes findings to a versioned file, does NOT modify source.
    #[default]
    Review,
    /// Act: agent can modify code, push commits (current/legacy behavior).
    Act,
}

/// Configuration for an automatic post-PR review worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewConfig {
    pub prompt: ReviewPrompt,
    /// Agent to use for the review. None = same as parent worker.
    pub agent: Option<AgentKind>,
    /// Extra instructions appended to the built-in prompt (from reviews.toml).
    #[serde(default)]
    pub extra_instructions: Option<String>,
    /// Review slug for dedup + display (e.g. "code-review", "security-audit").
    #[serde(default)]
    pub slug: Option<String>,
    /// Review mode (default: review = read-only).
    #[serde(default)]
    pub mode: ReviewMode,
}

/// A review prompt — either a built-in template or a custom file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewPrompt {
    BuiltIn { slug: String },
    Custom { filename: String, content: String },
}

// ── Built-in prompt templates ──────────────────────────────

const CODE_REVIEW_PROMPT: &str = r#"You are a code reviewer. A PR has been opened:

- **PR**: {{PR_TITLE}} ({{PR_URL}})

Your job:
1. Run `gh pr diff {{PR_NUMBER}}` to see the full diff.
2. Review the changes for bugs, logic errors, style issues, and missing edge cases.
3. Post a summary comment with `gh pr comment {{PR_NUMBER}} --body "<your review>"`.

Focus on substantive issues, not nitpicks. If everything looks good, say so."#;

const SECURITY_AUDIT_PROMPT: &str = r#"You are a security auditor. A PR has been opened:

- **PR**: {{PR_TITLE}} ({{PR_URL}})

Your job:
1. Run `gh pr diff {{PR_NUMBER}}` to see the full diff.
2. Check for security issues: injection vulnerabilities, authentication/authorization flaws, secrets or credentials in code, unsafe deserialization, path traversal, and OWASP top 10.
3. Post a summary with `gh pr comment {{PR_NUMBER}} --body "<your audit>"`.

Be thorough but avoid false positives."#;

const TEST_COVERAGE_PROMPT: &str = r#"You are a test engineer. A PR has been opened:

- **PR**: {{PR_TITLE}} ({{PR_URL}})

Your job:
1. Run `gh pr diff {{PR_NUMBER}}` to see what changed.
2. Identify code paths that lack test coverage.
3. Assess what tests are needed — unit tests, integration tests, or both.
4. Post a summary with `gh pr comment {{PR_NUMBER}} --body "<your coverage report>"`.

Focus on meaningful gaps, not boilerplate."#;

// ── Project-level review config ────────────────────────────

/// Project-level review config loaded from `.swarm/reviews.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectReviewConfig {
    /// Slugs that auto-trigger on PR detection.
    #[serde(default)]
    pub auto: Vec<String>,
    /// Per-slug overrides.
    #[serde(default)]
    pub reviews: HashMap<String, ReviewEntry>,
}

/// Per-slug review entry in `reviews.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct ReviewEntry {
    /// Extra instructions appended to the built-in prompt.
    pub prompt: Option<String>,
    /// Path relative to `.swarm/` for a custom prompt file.
    pub prompt_file: Option<String>,
    /// Agent override ("claude", "claude-tui").
    pub agent: Option<String>,
    /// Future: block merge until this review is done.
    #[serde(default)]
    pub blocking: bool,
    /// Review mode override (default: review = read-only).
    #[serde(default)]
    pub mode: Option<ReviewMode>,
}

/// Load `.swarm/reviews.toml` from the workspace directory.
pub fn load_reviews_toml(work_dir: &Path) -> Option<ProjectReviewConfig> {
    let path = work_dir.join(".swarm").join("reviews.toml");
    let content = std::fs::read_to_string(&path).ok()?;
    toml::from_str(&content).ok()
}

// ── Review output versioning ──────────────────────────────

/// Directory for review outputs: `.swarm/reviews/<parent_id>/<slug>/`
fn review_output_dir(work_dir: &Path, parent_id: &str, slug: &str) -> PathBuf {
    work_dir
        .join(".swarm")
        .join("reviews")
        .join(parent_id)
        .join(slug)
}

/// Scan `.swarm/reviews/<parent>/<slug>/v*.md` and return the next version number.
pub fn next_review_version(work_dir: &Path, parent_id: &str, slug: &str) -> u32 {
    let dir = review_output_dir(work_dir, parent_id, slug);
    let max = std::fs::read_dir(&dir)
        .ok()
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    // Match "v<N>.md"
                    name.strip_prefix('v')
                        .and_then(|rest| rest.strip_suffix(".md"))
                        .and_then(|n| n.parse::<u32>().ok())
                })
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0);
    max + 1
}

/// Full path for a review output file: `.swarm/reviews/<parent>/<slug>/v<N>.md`
pub fn review_output_path(work_dir: &Path, parent_id: &str, slug: &str, version: u32) -> PathBuf {
    review_output_dir(work_dir, parent_id, slug).join(format!("v{}.md", version))
}

/// Find the latest review output file (highest version), if any.
pub fn latest_review_output(work_dir: &Path, parent_id: &str, slug: &str) -> Option<PathBuf> {
    let dir = review_output_dir(work_dir, parent_id, slug);
    let max_version = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.strip_prefix('v')
                .and_then(|rest| rest.strip_suffix(".md"))
                .and_then(|n| n.parse::<u32>().ok())
        })
        .max()?;
    Some(dir.join(format!("v{}.md", max_version)))
}

/// Wrap a review prompt with mode-specific framing.
///
/// - `Review` mode: prepends read-only instructions, appends output path directive.
/// - `Act` mode: returns prompt unchanged (agent can modify code freely).
pub fn wrap_review_mode_prompt(prompt: &str, mode: ReviewMode, output_path: Option<&Path>) -> String {
    match mode {
        ReviewMode::Review => {
            let mut wrapped = String::from(
                "READ-ONLY REVIEW MODE: Do NOT modify any source files. Do NOT push commits. \
                 Do NOT run `git add`, `git commit`, or `git push`. \
                 Your only job is to analyze the code and write your findings.\n\n",
            );
            wrapped.push_str(prompt);
            if let Some(path) = output_path {
                wrapped.push_str(&format!(
                    "\n\nWrite your complete findings to `{}`. Create the file and any parent directories if they don't exist.",
                    path.display()
                ));
            }
            wrapped
        }
        ReviewMode::Act => {
            let mut wrapped = prompt.to_string();
            wrapped.push_str(
                "\n\nACT MODE: If you find issues, fix them directly — edit the code, \
                 commit your changes, and push to this branch.",
            );
            wrapped
        }
    }
}

/// Build `ReviewConfig`s for all `auto` slugs in `reviews.toml`.
/// Returns `(slug, ReviewConfig)` pairs ready for spawning.
pub fn resolve_auto_reviews(work_dir: &Path) -> Vec<(String, ReviewConfig)> {
    let project = match load_reviews_toml(work_dir) {
        Some(p) => p,
        None => return Vec::new(),
    };

    project
        .auto
        .iter()
        .filter_map(|slug| {
            resolve_single_review(slug, &project, work_dir)
                .map(|config| (slug.clone(), config))
        })
        .collect()
}

/// Resolve a single review slug into a `ReviewConfig`, considering
/// overrides from the project config.
pub fn resolve_single_review(
    slug: &str,
    project: &ProjectReviewConfig,
    work_dir: &Path,
) -> Option<ReviewConfig> {
    let entry = project.reviews.get(slug);

    // Determine the prompt: if the entry has a prompt_file, use that as Custom.
    // Otherwise use from_slug (built-in or .swarm/prompts/*.md).
    let prompt = if let Some(e) = entry
        && let Some(ref pf) = e.prompt_file
    {
        let file_path = work_dir.join(".swarm").join(pf);
        let content = std::fs::read_to_string(&file_path).ok()?;
        ReviewPrompt::Custom {
            filename: slug.to_string(),
            content,
        }
    } else {
        ReviewPrompt::from_slug(slug, work_dir)?
    };

    let agent = entry
        .and_then(|e| e.agent.as_ref())
        .and_then(|a| AgentKind::from_str(a));

    let extra_instructions = entry.and_then(|e| e.prompt.clone());

    let mode = entry.and_then(|e| e.mode).unwrap_or_default();

    Some(ReviewConfig {
        prompt,
        agent,
        extra_instructions,
        slug: Some(slug.to_string()),
        mode,
    })
}

impl ReviewPrompt {
    /// Extract the slug from either variant.
    pub fn slug(&self) -> &str {
        match self {
            Self::BuiltIn { slug } => slug.as_str(),
            Self::Custom { filename, .. } => filename.as_str(),
        }
    }

    /// Display label for the picker.
    pub fn label(&self) -> &str {
        match self {
            Self::BuiltIn { slug } => match slug.as_str() {
                "code-review" => "Code Review",
                "security-audit" => "Security Audit",
                "test-coverage" => "Test Coverage",
                _ => slug.as_str(),
            },
            Self::Custom { filename, .. } => filename.as_str(),
        }
    }

    /// Expand the prompt template with PR context.
    pub fn prompt_text(&self, pr_url: &str, pr_title: &str, pr_number: u64) -> String {
        let template = match self {
            Self::BuiltIn { slug } => match slug.as_str() {
                "code-review" => CODE_REVIEW_PROMPT,
                "security-audit" => SECURITY_AUDIT_PROMPT,
                "test-coverage" => TEST_COVERAGE_PROMPT,
                _ => return format!("Review PR: {pr_url}"),
            },
            Self::Custom { content, .. } => content.as_str(),
        };

        template
            .replace("{{PR_URL}}", pr_url)
            .replace("{{PR_TITLE}}", pr_title)
            .replace("{{PR_NUMBER}}", &pr_number.to_string())
    }

    /// Expand prompt with PR context, then append extra instructions if present.
    pub fn prompt_text_with_extra(
        &self,
        pr_url: &str,
        pr_title: &str,
        pr_number: u64,
        extra: Option<&str>,
    ) -> String {
        let mut text = self.prompt_text(pr_url, pr_title, pr_number);
        if let Some(extra) = extra {
            text.push_str("\n\n");
            text.push_str(extra);
        }
        text
    }

    /// All available review prompts: 3 built-ins + custom files from `.swarm/prompts/*.md`.
    pub fn available(work_dir: &Path) -> Vec<Self> {
        let mut prompts = vec![
            Self::BuiltIn {
                slug: "code-review".to_string(),
            },
            Self::BuiltIn {
                slug: "security-audit".to_string(),
            },
            Self::BuiltIn {
                slug: "test-coverage".to_string(),
            },
        ];

        // Scan for custom prompt files
        let prompts_dir = work_dir.join(".swarm").join("prompts");
        if let Ok(entries) = std::fs::read_dir(&prompts_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("md")
                    && let Ok(content) = std::fs::read_to_string(&path)
                {
                    let filename = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("custom")
                        .to_string();
                    prompts.push(Self::Custom { filename, content });
                }
            }
        }

        prompts
    }

    /// Look up a review prompt by slug (for CLI `--review` flag).
    pub fn from_slug(slug: &str, work_dir: &Path) -> Option<Self> {
        let all = Self::available(work_dir);
        all.into_iter().find(|p| match p {
            Self::BuiltIn { slug: s } => s == slug,
            Self::Custom { filename, .. } => filename == slug,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_labels() {
        let cr = ReviewPrompt::BuiltIn {
            slug: "code-review".to_string(),
        };
        assert_eq!(cr.label(), "Code Review");

        let sa = ReviewPrompt::BuiltIn {
            slug: "security-audit".to_string(),
        };
        assert_eq!(sa.label(), "Security Audit");

        let tc = ReviewPrompt::BuiltIn {
            slug: "test-coverage".to_string(),
        };
        assert_eq!(tc.label(), "Test Coverage");
    }

    #[test]
    fn prompt_text_substitution() {
        let prompt = ReviewPrompt::BuiltIn {
            slug: "code-review".to_string(),
        };
        let text = prompt.prompt_text(
            "https://github.com/org/repo/pull/42",
            "Fix auth bug",
            42,
        );
        assert!(text.contains("https://github.com/org/repo/pull/42"));
        assert!(text.contains("Fix auth bug"));
        assert!(text.contains("42"));
        assert!(!text.contains("{{PR_URL}}"));
        assert!(!text.contains("{{PR_TITLE}}"));
        assert!(!text.contains("{{PR_NUMBER}}"));
    }

    #[test]
    fn available_returns_three_builtins() {
        let dir = tempfile::tempdir().unwrap();
        let prompts = ReviewPrompt::available(dir.path());
        assert_eq!(prompts.len(), 3);
    }

    #[test]
    fn available_picks_up_custom_files() {
        let dir = tempfile::tempdir().unwrap();
        let prompts_dir = dir.path().join(".swarm").join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        std::fs::write(
            prompts_dir.join("my-review.md"),
            "Review {{PR_URL}} please",
        )
        .unwrap();

        let prompts = ReviewPrompt::available(dir.path());
        assert_eq!(prompts.len(), 4);

        let custom = &prompts[3];
        assert_eq!(custom.label(), "my-review");
        let text = custom.prompt_text("http://example.com", "Test", 1);
        assert!(text.contains("http://example.com"));
    }

    #[test]
    fn from_slug_finds_builtin() {
        let dir = tempfile::tempdir().unwrap();
        let found = ReviewPrompt::from_slug("code-review", dir.path());
        assert!(found.is_some());
        assert_eq!(found.unwrap().label(), "Code Review");
    }

    #[test]
    fn from_slug_finds_custom() {
        let dir = tempfile::tempdir().unwrap();
        let prompts_dir = dir.path().join(".swarm").join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        std::fs::write(prompts_dir.join("custom-check.md"), "check it").unwrap();

        let found = ReviewPrompt::from_slug("custom-check", dir.path());
        assert!(found.is_some());
    }

    #[test]
    fn from_slug_returns_none_for_unknown() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ReviewPrompt::from_slug("nonexistent", dir.path()).is_none());
    }

    #[test]
    fn review_config_round_trips() {
        let config = ReviewConfig {
            prompt: ReviewPrompt::BuiltIn {
                slug: "code-review".to_string(),
            },
            agent: Some(AgentKind::Claude),
            extra_instructions: None,
            slug: None,
            mode: ReviewMode::default(),
        };
        let json = serde_json::to_string(&config).unwrap();
        let restored: ReviewConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.prompt.label(), "Code Review");
    }

    #[test]
    fn review_config_with_extra_fields_round_trips() {
        let config = ReviewConfig {
            prompt: ReviewPrompt::BuiltIn {
                slug: "security-audit".to_string(),
            },
            agent: None,
            extra_instructions: Some("Also check HIPAA compliance.".to_string()),
            slug: Some("security-audit".to_string()),
            mode: ReviewMode::Act,
        };
        let json = serde_json::to_string(&config).unwrap();
        let restored: ReviewConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(
            restored.extra_instructions.as_deref(),
            Some("Also check HIPAA compliance.")
        );
        assert_eq!(restored.slug.as_deref(), Some("security-audit"));
    }

    #[test]
    fn old_review_config_without_new_fields_deserializes() {
        let json = r#"{"prompt":{"kind":"built_in","slug":"code-review"},"agent":null}"#;
        let config: ReviewConfig = serde_json::from_str(json).unwrap();
        assert!(config.extra_instructions.is_none());
        assert!(config.slug.is_none());
    }

    #[test]
    fn slug_method() {
        let builtin = ReviewPrompt::BuiltIn {
            slug: "code-review".to_string(),
        };
        assert_eq!(builtin.slug(), "code-review");

        let custom = ReviewPrompt::Custom {
            filename: "my-review".to_string(),
            content: "check it".to_string(),
        };
        assert_eq!(custom.slug(), "my-review");
    }

    #[test]
    fn prompt_text_with_extra_appends() {
        let prompt = ReviewPrompt::BuiltIn {
            slug: "code-review".to_string(),
        };
        let text = prompt.prompt_text_with_extra(
            "https://github.com/org/repo/pull/42",
            "Fix auth bug",
            42,
            Some("Also check for PII."),
        );
        assert!(text.contains("gh pr diff 42"));
        assert!(text.ends_with("Also check for PII."));
    }

    #[test]
    fn prompt_text_with_extra_none() {
        let prompt = ReviewPrompt::BuiltIn {
            slug: "code-review".to_string(),
        };
        let without_extra = prompt.prompt_text(
            "https://github.com/org/repo/pull/42",
            "Fix auth bug",
            42,
        );
        let with_none = prompt.prompt_text_with_extra(
            "https://github.com/org/repo/pull/42",
            "Fix auth bug",
            42,
            None,
        );
        assert_eq!(without_extra, with_none);
    }

    // ── ProjectReviewConfig tests ──────────────────────────

    #[test]
    fn load_reviews_toml_basic() {
        let dir = tempfile::tempdir().unwrap();
        let swarm_dir = dir.path().join(".swarm");
        std::fs::create_dir_all(&swarm_dir).unwrap();
        std::fs::write(
            swarm_dir.join("reviews.toml"),
            r#"
auto = ["code-review", "security-audit"]

[reviews.security-audit]
prompt = "Also check HIPAA."
blocking = true

[reviews.compliance]
prompt_file = "prompts/compliance.md"
"#,
        )
        .unwrap();

        let config = load_reviews_toml(dir.path()).unwrap();
        assert_eq!(config.auto, vec!["code-review", "security-audit"]);
        assert_eq!(config.reviews.len(), 2);

        let sec = &config.reviews["security-audit"];
        assert_eq!(sec.prompt.as_deref(), Some("Also check HIPAA."));
        assert!(sec.blocking);

        let comp = &config.reviews["compliance"];
        assert_eq!(comp.prompt_file.as_deref(), Some("prompts/compliance.md"));
        assert!(!comp.blocking);
    }

    #[test]
    fn load_reviews_toml_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_reviews_toml(dir.path()).is_none());
    }

    #[test]
    fn resolve_auto_reviews_basic() {
        let dir = tempfile::tempdir().unwrap();
        let swarm_dir = dir.path().join(".swarm");
        std::fs::create_dir_all(&swarm_dir).unwrap();
        std::fs::write(
            swarm_dir.join("reviews.toml"),
            r#"
auto = ["code-review", "security-audit"]

[reviews.security-audit]
prompt = "Also check HIPAA."
"#,
        )
        .unwrap();

        let configs = resolve_auto_reviews(dir.path());
        assert_eq!(configs.len(), 2);

        let (slug0, config0) = &configs[0];
        assert_eq!(slug0, "code-review");
        assert!(config0.extra_instructions.is_none());
        assert_eq!(config0.slug.as_deref(), Some("code-review"));

        let (slug1, config1) = &configs[1];
        assert_eq!(slug1, "security-audit");
        assert_eq!(
            config1.extra_instructions.as_deref(),
            Some("Also check HIPAA.")
        );
    }

    #[test]
    fn resolve_auto_reviews_skips_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let swarm_dir = dir.path().join(".swarm");
        std::fs::create_dir_all(&swarm_dir).unwrap();
        std::fs::write(
            swarm_dir.join("reviews.toml"),
            r#"auto = ["code-review", "nonexistent-slug"]"#,
        )
        .unwrap();

        let configs = resolve_auto_reviews(dir.path());
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].0, "code-review");
    }

    #[test]
    fn resolve_auto_reviews_no_toml() {
        let dir = tempfile::tempdir().unwrap();
        let configs = resolve_auto_reviews(dir.path());
        assert!(configs.is_empty());
    }

    #[test]
    fn resolve_single_review_with_prompt_file() {
        let dir = tempfile::tempdir().unwrap();
        let swarm_dir = dir.path().join(".swarm");
        let prompts_dir = swarm_dir.join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        std::fs::write(
            prompts_dir.join("compliance.md"),
            "Check compliance for {{PR_URL}}",
        )
        .unwrap();

        let project = ProjectReviewConfig {
            auto: vec![],
            reviews: {
                let mut m = HashMap::new();
                m.insert(
                    "compliance".to_string(),
                    ReviewEntry {
                        prompt: None,
                        prompt_file: Some("prompts/compliance.md".to_string()),
                        agent: None,
                        blocking: false,
                        mode: None,
                    },
                );
                m
            },
        };

        let config = resolve_single_review("compliance", &project, dir.path()).unwrap();
        assert_eq!(config.slug.as_deref(), Some("compliance"));
        let text = config
            .prompt
            .prompt_text("http://example.com", "Test", 1);
        assert!(text.contains("http://example.com"));
    }

    #[test]
    fn resolve_single_review_with_agent_override() {
        let dir = tempfile::tempdir().unwrap();
        let project = ProjectReviewConfig {
            auto: vec![],
            reviews: {
                let mut m = HashMap::new();
                m.insert(
                    "code-review".to_string(),
                    ReviewEntry {
                        prompt: Some("Be extra strict.".to_string()),
                        prompt_file: None,
                        agent: Some("claude".to_string()),
                        blocking: false,
                        mode: None,
                    },
                );
                m
            },
        };

        let config = resolve_single_review("code-review", &project, dir.path()).unwrap();
        assert_eq!(config.agent, Some(AgentKind::Claude));
        assert_eq!(
            config.extra_instructions.as_deref(),
            Some("Be extra strict.")
        );
    }

    // ── ReviewMode tests ──────────────────────────────────

    #[test]
    fn review_mode_default_is_review() {
        assert_eq!(ReviewMode::default(), ReviewMode::Review);
    }

    #[test]
    fn review_mode_serde_round_trip() {
        let review = ReviewMode::Review;
        let json = serde_json::to_string(&review).unwrap();
        assert_eq!(json, "\"review\"");
        let restored: ReviewMode = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, ReviewMode::Review);

        let act = ReviewMode::Act;
        let json = serde_json::to_string(&act).unwrap();
        assert_eq!(json, "\"act\"");
        let restored: ReviewMode = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, ReviewMode::Act);
    }

    #[test]
    fn resolve_single_review_propagates_mode() {
        let dir = tempfile::tempdir().unwrap();
        let project = ProjectReviewConfig {
            auto: vec![],
            reviews: {
                let mut m = HashMap::new();
                m.insert(
                    "code-review".to_string(),
                    ReviewEntry {
                        prompt: None,
                        prompt_file: None,
                        agent: None,
                        blocking: false,
                        mode: Some(ReviewMode::Act),
                    },
                );
                m
            },
        };

        let config = resolve_single_review("code-review", &project, dir.path()).unwrap();
        assert_eq!(config.mode, ReviewMode::Act);
    }

    #[test]
    fn resolve_single_review_defaults_to_review_mode() {
        let dir = tempfile::tempdir().unwrap();
        let project = ProjectReviewConfig {
            auto: vec![],
            reviews: {
                let mut m = HashMap::new();
                m.insert(
                    "code-review".to_string(),
                    ReviewEntry {
                        prompt: None,
                        prompt_file: None,
                        agent: None,
                        blocking: false,
                        mode: None,
                    },
                );
                m
            },
        };

        let config = resolve_single_review("code-review", &project, dir.path()).unwrap();
        assert_eq!(config.mode, ReviewMode::Review);
    }

    // ── Version helpers tests ─────────────────────────────

    #[test]
    fn next_review_version_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(next_review_version(dir.path(), "hive-1", "code-review"), 1);
    }

    #[test]
    fn next_review_version_with_existing() {
        let dir = tempfile::tempdir().unwrap();
        let review_dir = dir
            .path()
            .join(".swarm/reviews/hive-1/code-review");
        std::fs::create_dir_all(&review_dir).unwrap();
        std::fs::write(review_dir.join("v1.md"), "findings v1").unwrap();
        std::fs::write(review_dir.join("v2.md"), "findings v2").unwrap();

        assert_eq!(next_review_version(dir.path(), "hive-1", "code-review"), 3);
    }

    #[test]
    fn review_output_path_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = review_output_path(dir.path(), "hive-1", "code-review", 3);
        assert!(path.ends_with(".swarm/reviews/hive-1/code-review/v3.md"));
    }

    #[test]
    fn latest_review_output_none_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(latest_review_output(dir.path(), "hive-1", "code-review").is_none());
    }

    #[test]
    fn latest_review_output_finds_highest() {
        let dir = tempfile::tempdir().unwrap();
        let review_dir = dir
            .path()
            .join(".swarm/reviews/hive-1/code-review");
        std::fs::create_dir_all(&review_dir).unwrap();
        std::fs::write(review_dir.join("v1.md"), "v1").unwrap();
        std::fs::write(review_dir.join("v3.md"), "v3").unwrap();
        std::fs::write(review_dir.join("v2.md"), "v2").unwrap();

        let latest = latest_review_output(dir.path(), "hive-1", "code-review").unwrap();
        assert!(latest.ends_with("v3.md"));
    }

    // ── deserialize_review_configs tests ────────────────────

    /// Helper struct that mirrors the serde attributes used on WorktreeState/InboxMessage
    #[derive(Debug, Deserialize)]
    struct TestContainer {
        #[serde(
            default,
            deserialize_with = "super::deserialize_review_configs",
            alias = "review_config"
        )]
        review_configs: Option<Vec<ReviewConfig>>,
    }

    #[test]
    fn deserialize_review_configs_old_single_object() {
        // Old format: "review_config": { single ReviewConfig }
        let json = r#"{"review_config":{"prompt":{"kind":"built_in","slug":"code-review"},"agent":null,"mode":"review"}}"#;
        let container: TestContainer = serde_json::from_str(json).unwrap();
        let configs = container.review_configs.expect("should be Some");
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].prompt.slug(), "code-review");
    }

    #[test]
    fn deserialize_review_configs_new_array() {
        let json = r#"{"review_configs":[
            {"prompt":{"kind":"built_in","slug":"code-review"},"agent":null,"mode":"review"},
            {"prompt":{"kind":"built_in","slug":"security-audit"},"agent":null,"mode":"review"}
        ]}"#;
        let container: TestContainer = serde_json::from_str(json).unwrap();
        let configs = container.review_configs.expect("should be Some");
        assert_eq!(configs.len(), 2);
        assert_eq!(configs[0].prompt.slug(), "code-review");
        assert_eq!(configs[1].prompt.slug(), "security-audit");
    }

    #[test]
    fn deserialize_review_configs_missing_field() {
        let json = r#"{}"#;
        let container: TestContainer = serde_json::from_str(json).unwrap();
        assert!(container.review_configs.is_none());
    }

    #[test]
    fn deserialize_review_configs_empty_array() {
        let json = r#"{"review_configs":[]}"#;
        let container: TestContainer = serde_json::from_str(json).unwrap();
        let configs = container.review_configs.expect("should be Some");
        assert!(configs.is_empty());
    }

    #[test]
    fn deserialize_review_configs_null() {
        let json = r#"{"review_configs":null}"#;
        let container: TestContainer = serde_json::from_str(json).unwrap();
        assert!(container.review_configs.is_none());
    }

    // ── wrap_review_mode_prompt tests ─────────────────────

    #[test]
    fn wrap_review_mode_review() {
        let path = PathBuf::from("/tmp/findings.md");
        let wrapped = wrap_review_mode_prompt("Review the code", ReviewMode::Review, Some(&path));
        assert!(wrapped.contains("READ-ONLY"));
        assert!(wrapped.contains("Review the code"));
        assert!(wrapped.contains("/tmp/findings.md"));
    }

    #[test]
    fn wrap_review_mode_act_appends_instructions() {
        let wrapped = wrap_review_mode_prompt("Fix the bugs", ReviewMode::Act, None);
        assert!(wrapped.starts_with("Fix the bugs"));
        assert!(wrapped.contains("ACT MODE"));
        assert!(wrapped.contains("fix them directly"));
        assert!(!wrapped.contains("READ-ONLY"));
    }
}
