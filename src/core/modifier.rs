use std::path::Path;

/// A prompt modifier — either a built-in behavior hint or a custom file.
#[derive(Debug, Clone)]
pub enum ModifierPrompt {
    BuiltIn {
        slug: String,
        label: String,
        content: String,
    },
    Custom {
        filename: String,
        content: String,
    },
}

impl ModifierPrompt {
    /// Extract the slug from either variant.
    pub fn slug(&self) -> &str {
        match self {
            Self::BuiltIn { slug, .. } => slug.as_str(),
            Self::Custom { filename, .. } => filename.as_str(),
        }
    }

    /// Display label for the picker.
    pub fn label(&self) -> &str {
        match self {
            Self::BuiltIn { label, .. } => label.as_str(),
            Self::Custom { filename, .. } => filename.as_str(),
        }
    }

    /// The modifier text to prepend.
    pub fn content(&self) -> &str {
        match self {
            Self::BuiltIn { content, .. } => content.as_str(),
            Self::Custom { content, .. } => content.as_str(),
        }
    }

    /// All available modifiers: built-ins + custom files from `.swarm/modifiers/*.md`.
    pub fn available(work_dir: &Path) -> Vec<Self> {
        let mut modifiers = vec![
            Self::BuiltIn {
                slug: "research-first".to_string(),
                label: "Research First".to_string(),
                content: "Before implementing, use web search to research best practices and current approaches for this task. Write your findings to a markdown file in the project, then proceed with implementation using what you learned.".to_string(),
            },
            Self::BuiltIn {
                slug: "explore-patterns".to_string(),
                label: "Explore Patterns".to_string(),
                content: "Before implementing, explore the surrounding codebase for similar patterns, conventions, and shared utilities. Reuse existing code where possible and follow established patterns for consistency.".to_string(),
            },
        ];

        // Scan for custom modifier files
        let modifiers_dir = work_dir.join(".swarm").join("modifiers");
        if let Ok(entries) = std::fs::read_dir(&modifiers_dir) {
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
                    modifiers.push(Self::Custom { filename, content });
                }
            }
        }

        modifiers
    }

    /// Look up a modifier by slug (for CLI `--mod` flag).
    pub fn from_slug(slug: &str, work_dir: &Path) -> Option<Self> {
        let all = Self::available(work_dir);
        all.into_iter().find(|m| m.slug() == slug)
    }
}

/// Assemble a final prompt from the base prompt and selected modifiers.
///
/// Format: `[mod1]\n\n---\n\n[mod2]\n\n---\n\n[user prompt]`
/// If no modifiers are selected, returns the base prompt unchanged.
pub fn assemble_prompt(base: &str, modifiers: &[ModifierPrompt], selected: &[bool]) -> String {
    let active: Vec<&str> = modifiers
        .iter()
        .zip(selected.iter())
        .filter(|&(_, sel)| *sel)
        .map(|(m, _)| m.content())
        .collect();

    if active.is_empty() {
        return base.to_string();
    }

    let mut parts: Vec<&str> = active;
    parts.push(base);
    parts.join("\n\n---\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_slug_and_label() {
        let dir = tempfile::tempdir().unwrap();
        let mods = ModifierPrompt::available(dir.path());
        assert_eq!(mods.len(), 2);
        assert_eq!(mods[0].slug(), "research-first");
        assert_eq!(mods[0].label(), "Research First");
        assert_eq!(mods[1].slug(), "explore-patterns");
        assert_eq!(mods[1].label(), "Explore Patterns");
    }

    #[test]
    fn available_picks_up_custom_files() {
        let dir = tempfile::tempdir().unwrap();
        let modifiers_dir = dir.path().join(".swarm").join("modifiers");
        std::fs::create_dir_all(&modifiers_dir).unwrap();
        std::fs::write(modifiers_dir.join("thorough.md"), "Be thorough.").unwrap();

        let mods = ModifierPrompt::available(dir.path());
        assert_eq!(mods.len(), 3);
        assert_eq!(mods[2].slug(), "thorough");
        assert_eq!(mods[2].label(), "thorough");
        assert_eq!(mods[2].content(), "Be thorough.");
    }

    #[test]
    fn available_without_custom_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mods = ModifierPrompt::available(dir.path());
        assert_eq!(mods.len(), 2);
    }

    #[test]
    fn from_slug_finds_builtin() {
        let dir = tempfile::tempdir().unwrap();
        let found = ModifierPrompt::from_slug("research-first", dir.path());
        assert!(found.is_some());
        assert_eq!(found.unwrap().label(), "Research First");
    }

    #[test]
    fn from_slug_finds_custom() {
        let dir = tempfile::tempdir().unwrap();
        let modifiers_dir = dir.path().join(".swarm").join("modifiers");
        std::fs::create_dir_all(&modifiers_dir).unwrap();
        std::fs::write(modifiers_dir.join("my-mod.md"), "custom content").unwrap();

        let found = ModifierPrompt::from_slug("my-mod", dir.path());
        assert!(found.is_some());
        assert_eq!(found.unwrap().content(), "custom content");
    }

    #[test]
    fn from_slug_returns_none_for_unknown() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ModifierPrompt::from_slug("nonexistent", dir.path()).is_none());
    }

    #[test]
    fn assemble_prompt_no_modifiers() {
        let mods = ModifierPrompt::available(&std::path::PathBuf::from("/tmp"));
        let selected = vec![false; mods.len()];
        let result = assemble_prompt("do the task", &mods, &selected);
        assert_eq!(result, "do the task");
    }

    #[test]
    fn assemble_prompt_single_modifier() {
        let mods = ModifierPrompt::available(&std::path::PathBuf::from("/tmp"));
        let mut selected = vec![false; mods.len()];
        selected[0] = true; // research-first
        let result = assemble_prompt("do the task", &mods, &selected);
        assert!(result.starts_with("Before implementing, use web search"));
        assert!(result.contains("\n\n---\n\n"));
        assert!(result.ends_with("do the task"));
    }

    #[test]
    fn assemble_prompt_multiple_modifiers() {
        let mods = ModifierPrompt::available(&std::path::PathBuf::from("/tmp"));
        let selected = vec![true; mods.len()];
        let result = assemble_prompt("do the task", &mods, &selected);
        // Should have: [mod1] --- [mod2] --- [base]
        let parts: Vec<&str> = result.split("\n\n---\n\n").collect();
        assert_eq!(parts.len(), 3);
        assert!(parts[0].contains("web search"));
        assert!(parts[1].contains("explore the surrounding codebase"));
        assert_eq!(parts[2], "do the task");
    }
}
