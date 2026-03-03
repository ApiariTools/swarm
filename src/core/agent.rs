use serde::{Deserialize, Serialize};

/// Supported agent types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Claude,
    Codex,
    #[serde(rename = "claude-tui")]
    ClaudeTui,
}

impl AgentKind {
    /// Parse from string.
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "claude-tui" => Some(Self::ClaudeTui),
            _ => None,
        }
    }

    /// Display name.
    pub fn name(&self) -> &str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::ClaudeTui => "Claude TUI",
        }
    }

    /// User-facing name for the daemon TUI (hides implementation details).
    pub fn daemon_name(&self) -> &str {
        match self {
            Self::Claude => "Claude Code",
            Self::ClaudeTui => "Claude",
            Self::Codex => "Codex",
        }
    }

    /// Short label for the TUI.
    pub fn label(&self) -> &str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::ClaudeTui => "claude-tui",
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
        assert_eq!(AgentKind::Claude.daemon_name(), "Claude Code");
        assert_eq!(AgentKind::ClaudeTui.daemon_name(), "Claude");
        assert_eq!(AgentKind::Codex.daemon_name(), "Codex");
    }
}
