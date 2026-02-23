use crate::core::shell::shell_quote;
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

    /// Short label for the TUI.
    pub fn label(&self) -> &str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::ClaudeTui => "claude-tui",
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
            Self::ClaudeTui => {
                let exe = std::env::current_exe()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "swarm".to_string());
                let mut cmd = format!("'{}' agent-tui", exe);
                if dangerously_skip {
                    cmd.push_str(" --dangerously-skip-permissions");
                }
                cmd
            }
        }
    }

    /// The shell command to launch this agent with an initial prompt baked in.
    pub fn launch_cmd_with_prompt(&self, prompt: &str, dangerously_skip: bool) -> String {
        let base = self.launch_cmd(dangerously_skip);
        if prompt.trim().is_empty() {
            return base;
        }
        match self {
            Self::ClaudeTui => {
                // agent-tui takes prompt as a positional arg
                format!("{} {}", base, shell_quote(prompt))
            }
            Self::Claude => format!("{} --max-turns 50 {}", base, shell_quote(prompt)),
            Self::Codex => format!("{} {}", base, shell_quote(prompt)),
        }
    }

    /// All available agents.
    pub fn all() -> Vec<Self> {
        vec![Self::ClaudeTui, Self::Claude, Self::Codex]
    }
}

impl std::fmt::Display for AgentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}
