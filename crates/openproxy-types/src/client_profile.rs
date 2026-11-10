use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Reusable client profile identities for upstream spoofing and formatting.
///
/// Allows arbitrary providers and combo targets (such as third-party resellers
/// or private gateways) to emulate the native CLI wire formats, system prompts,
/// billing headers, and capabilities expected by strict model providers (e.g.
/// Anthropic Claude Code, OpenAI Codex, OpenCode, Cline, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClientProfile {
    ClaudeCode,
    Codex,
    Cline,
    Kilocode,
    Kiro,
    Minimax,
    #[serde(rename = "commandcode", alias = "command-code")]
    CommandCode,
    #[serde(rename = "codebuddy", alias = "code-buddy")]
    CodeBuddy,
    Opencode,
    Antigravity,
}

impl ClientProfile {
    pub const ALL: &'static [Self] = &[
        Self::ClaudeCode,
        Self::Codex,
        Self::Cline,
        Self::Kilocode,
        Self::Kiro,
        Self::Minimax,
        Self::CommandCode,
        Self::CodeBuddy,
        Self::Opencode,
        Self::Antigravity,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Cline => "cline",
            Self::Kilocode => "kilocode",
            Self::Kiro => "kiro",
            Self::Minimax => "minimax",
            Self::CommandCode => "commandcode",
            Self::CodeBuddy => "codebuddy",
            Self::Opencode => "opencode",
            Self::Antigravity => "antigravity",
        }
    }
}

impl fmt::Display for ClientProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for ClientProfile {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = s.trim().to_ascii_lowercase().replace('_', "-");
        match normalized.as_str() {
            "claude-code" | "claude" | "claudecode" => Ok(Self::ClaudeCode),
            "codex" => Ok(Self::Codex),
            "cline" => Ok(Self::Cline),
            "kilocode" => Ok(Self::Kilocode),
            "kiro" => Ok(Self::Kiro),
            "minimax" => Ok(Self::Minimax),
            "commandcode" | "cmd" | "command-code" => Ok(Self::CommandCode),
            "codebuddy" | "code-buddy" => Ok(Self::CodeBuddy),
            "opencode" => Ok(Self::Opencode),
            "antigravity" => Ok(Self::Antigravity),
            other => Err(format!("unknown client profile: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip() {
        for profile in ClientProfile::ALL {
            let json = serde_json::to_string(profile).unwrap();
            assert_eq!(json, format!("\"{}\"", profile.as_str()));
            let back: ClientProfile = serde_json::from_str(&json).unwrap();
            assert_eq!(*profile, back);
        }
    }

    #[test]
    fn parse_aliases() {
        assert_eq!("claude".parse::<ClientProfile>().unwrap(), ClientProfile::ClaudeCode);
        assert_eq!("claude-code".parse::<ClientProfile>().unwrap(), ClientProfile::ClaudeCode);
        assert_eq!("cmd".parse::<ClientProfile>().unwrap(), ClientProfile::CommandCode);
        assert_eq!("code_buddy".parse::<ClientProfile>().unwrap(), ClientProfile::CodeBuddy);
        assert!("unknown".parse::<ClientProfile>().is_err());
    }
}
