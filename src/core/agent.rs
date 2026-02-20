use serde::{Deserialize, Serialize};

/// Supported agent types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Claude,
    Codex,
}

impl AgentKind {
    /// Parse from string.
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    /// Display name.
    pub fn name(&self) -> &str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
        }
    }

    /// Short label for the TUI.
    pub fn label(&self) -> &str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// The shell command to launch this agent (interactive, no prompt baked in).
    pub fn launch_cmd(&self, dangerously_skip: bool) -> String {
        match self {
            Self::Claude => {
                let flags = if dangerously_skip {
                    " --dangerously-skip-permissions"
                } else {
                    ""
                };
                format!("claude{}", flags)
            }
            Self::Codex => "codex".to_string(),
        }
    }

    /// All available agents.
    pub fn all() -> Vec<Self> {
        vec![Self::Claude, Self::Codex]
    }
}

impl std::fmt::Display for AgentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

