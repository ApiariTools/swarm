use crate::core::agent::AgentKind;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

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

// ── Project-level review config ────────────────────────────

/// Project-level review config loaded from `.swarm/reviews.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectReviewConfig {
    /// Slugs that auto-trigger on PR detection.
    #[serde(default)]
    #[allow(dead_code)] // deserialized from reviews.toml; used in tests
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
    /// Agent override ("claude", "claude-tui").
    pub agent: Option<String>,
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

[reviews.compliance]
agent = "claude"
"#,
        )
        .unwrap();

        let config = load_reviews_toml(dir.path()).unwrap();
        assert_eq!(config.auto, vec!["code-review", "security-audit"]);
        assert_eq!(config.reviews.len(), 2);

        let sec = &config.reviews["security-audit"];
        assert_eq!(sec.prompt.as_deref(), Some("Also check HIPAA."));
    }

    #[test]
    fn load_reviews_toml_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_reviews_toml(dir.path()).is_none());
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
}
