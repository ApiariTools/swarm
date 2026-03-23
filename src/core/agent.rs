use serde::{Deserialize, Serialize};

/// Supported agent types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    /// Claude Code agent (via SDK). Also deserializes from "claude-tui" for
    /// backward compatibility with existing state files.
    #[serde(alias = "claude-tui")]
    Claude,
    Codex,
    Gemini,
}

impl AgentKind {
    /// Parse from string.
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "claude" | "claude-tui" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "gemini" => Some(Self::Gemini),
            _ => None,
        }
    }

    /// Display name.
    pub fn name(&self) -> &str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Gemini => "Gemini",
        }
    }

    /// User-facing name for the daemon TUI.
    pub fn daemon_name(&self) -> &str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Gemini => "Gemini",
        }
    }

    /// Short label for the TUI.
    pub fn label(&self) -> &str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
        }
    }
}

impl std::fmt::Display for AgentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_name_values() {
        assert_eq!(AgentKind::Claude.daemon_name(), "Claude");
        assert_eq!(AgentKind::Codex.daemon_name(), "Codex");
        assert_eq!(AgentKind::Gemini.daemon_name(), "Gemini");
    }

    #[test]
    fn from_str_backward_compat() {
        // "claude-tui" should still parse as Claude
        assert_eq!(AgentKind::from_str("claude-tui"), Some(AgentKind::Claude));
        assert_eq!(AgentKind::from_str("claude"), Some(AgentKind::Claude));
    }

    #[test]
    fn deserialize_claude_tui_alias() {
        let json = r#""claude-tui""#;
        let kind: AgentKind = serde_json::from_str(json).unwrap();
        assert_eq!(kind, AgentKind::Claude);
    }
}
