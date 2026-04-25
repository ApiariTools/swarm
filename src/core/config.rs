use serde::Deserialize;
use std::path::Path;

/// Workspace-level swarm configuration, read from `.swarm/config.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct SwarmConfig {
    #[serde(default)]
    #[allow(dead_code)]
    pub default_agent: Option<String>,

    /// When true (the default), workers are automatically closed when their PR
    /// is merged. Set to false to keep the worker alive after merge.
    #[serde(default = "default_true")]
    pub close_on_pr_merge: bool,
}

fn default_true() -> bool {
    true
}

impl Default for SwarmConfig {
    fn default() -> Self {
        Self {
            default_agent: None,
            close_on_pr_merge: true,
        }
    }
}

/// Load the swarm config from `.swarm/config.toml` in the given workspace.
/// Returns `SwarmConfig::default()` if the file is missing or unparseable.
pub fn load_config(workspace_path: &Path) -> SwarmConfig {
    let config_path = workspace_path.join(".swarm").join("config.toml");
    match std::fs::read_to_string(&config_path) {
        Ok(content) => match toml::from_str(&content) {
            Ok(config) => config,
            Err(e) => {
                tracing::warn!(path = %config_path.display(), error = %e, "Failed to parse config.toml, using defaults");
                SwarmConfig::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SwarmConfig::default(),
        Err(e) => {
            tracing::warn!(path = %config_path.display(), error = %e, "Failed to read config.toml, using defaults");
            SwarmConfig::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_close_on_pr_merge_true() {
        let config = SwarmConfig::default();
        assert!(config.close_on_pr_merge);
    }

    #[test]
    fn parse_config_without_close_on_pr_merge() {
        let toml_str = r#"default_agent = "claude""#;
        let config: SwarmConfig = toml::from_str(toml_str).unwrap();
        assert!(config.close_on_pr_merge);
        assert_eq!(config.default_agent.as_deref(), Some("claude"));
    }

    #[test]
    fn parse_config_with_close_on_pr_merge_false() {
        let toml_str = r#"
default_agent = "claude"
close_on_pr_merge = false
"#;
        let config: SwarmConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.close_on_pr_merge);
    }

    #[test]
    fn parse_config_with_close_on_pr_merge_true() {
        let toml_str = r#"close_on_pr_merge = true"#;
        let config: SwarmConfig = toml::from_str(toml_str).unwrap();
        assert!(config.close_on_pr_merge);
    }

    #[test]
    fn load_config_missing_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let config = load_config(dir.path());
        assert!(config.close_on_pr_merge);
    }

    #[test]
    fn parse_empty_config() {
        let config: SwarmConfig = toml::from_str("").unwrap();
        assert!(config.close_on_pr_merge);
        assert!(config.default_agent.is_none());
    }
}
