use crate::config::CooldownMode;
use crate::ids::{AccountId, ComboId, ComboTargetId, ModelRowId, ProviderId};
use serde::{Deserialize, Serialize};

pub const MAX_SUB_COMBO_DEPTH: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    Priority,
    RoundRobin,
    Shuffle,
}

impl Strategy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Priority => "priority",
            Self::RoundRobin => "round_robin",
            Self::Shuffle => "shuffle",
        }
    }
    pub fn parse(s: &str) -> std::result::Result<Self, String> {
        match s {
            "priority" => Ok(Self::Priority),
            "round_robin" => Ok(Self::RoundRobin),
            "shuffle" => Ok(Self::Shuffle),
            other => Err(format!("invalid strategy: {}", other)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PriorityMode {
    #[default]
    Strict,
    Lkgp,
    Weighted,
    LeastUsed,
    P2c,
}

impl PriorityMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Lkgp => "lkgp",
            Self::Weighted => "weighted",
            Self::LeastUsed => "least_used",
            Self::P2c => "p2c",
        }
    }
    pub fn parse(s: &str) -> std::result::Result<Self, String> {
        match s {
            "strict" => Ok(Self::Strict),
            "lkgp" => Ok(Self::Lkgp),
            "weighted" => Ok(Self::Weighted),
            "least_used" => Ok(Self::LeastUsed),
            "p2c" => Ok(Self::P2c),
            other => Err(format!("invalid priority_mode: {}", other)),
        }
    }
    pub fn from_db(s: Option<&str>) -> Self {
        match s {
            Some("lkgp") => Self::Lkgp,
            Some("weighted") => Self::Weighted,
            Some("least_used") => Self::LeastUsed,
            Some("p2c") => Self::P2c,
            _ => Self::Strict,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Combo {
    pub id: ComboId,
    pub name: String,
    pub strategy: Strategy,
    pub race_size: u8,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<i64>,
    #[serde(default)]
    pub priority_mode: PriorityMode,
    #[serde(default)]
    pub cooldown_mode: CooldownMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_base_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_max_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_factor: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lkgp_exploration_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_window_secs: Option<u64>,
}

fn default_target_weight() -> i32 {
    1
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComboTarget {
    pub id: ComboTargetId,
    pub combo_id: ComboId,
    pub provider_id: ProviderId,
    pub account_id: Option<AccountId>,
    pub model_row_id: Option<ModelRowId>,
    pub sub_combo_id: Option<ComboId>,
    pub priority_order: i32,
    #[serde(default = "default_target_weight")]
    pub weight: i32,
    #[serde(default)]
    pub rate_limit_scope: crate::providers::RateLimitScope,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComboTargetWithModel {
    pub id: ComboTargetId,
    pub combo_id: ComboId,
    pub provider_id: ProviderId,
    pub account_id: Option<AccountId>,
    pub model_row_id: Option<ModelRowId>,
    pub sub_combo_id: Option<ComboId>,
    pub sub_combo_name: Option<String>,
    pub model_id: String,
    pub model_display_name: Option<String>,
    pub priority_order: i32,
    #[serde(default = "default_target_weight")]
    pub weight: i32,
    #[serde(default)]
    pub in_cooldown: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i64>,
    #[serde(default = "default_true")]
    pub provider_active: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strategy_parse() {
        assert_eq!(Strategy::parse("priority"), Ok(Strategy::Priority));
        assert_eq!(Strategy::parse("round_robin"), Ok(Strategy::RoundRobin));
        assert_eq!(Strategy::parse("shuffle"), Ok(Strategy::Shuffle));
        assert_eq!(Strategy::parse("unknown"), Err("invalid strategy: unknown".to_string()));
    }

    #[test]
    fn test_strategy_as_str() {
        assert_eq!(Strategy::Priority.as_str(), "priority");
        assert_eq!(Strategy::RoundRobin.as_str(), "round_robin");
        assert_eq!(Strategy::Shuffle.as_str(), "shuffle");
    }

    #[test]
    fn test_priority_mode_parse() {
        assert_eq!(PriorityMode::parse("strict"), Ok(PriorityMode::Strict));
        assert_eq!(PriorityMode::parse("lkgp"), Ok(PriorityMode::Lkgp));
        assert_eq!(PriorityMode::parse("weighted"), Ok(PriorityMode::Weighted));
        assert_eq!(
            PriorityMode::parse("least_used"),
            Ok(PriorityMode::LeastUsed)
        );
        assert_eq!(PriorityMode::parse("p2c"), Ok(PriorityMode::P2c));
        assert!(PriorityMode::parse("unknown").is_err());
    }

    #[test]
    fn test_priority_mode_as_str() {
        assert_eq!(PriorityMode::Strict.as_str(), "strict");
        assert_eq!(PriorityMode::Lkgp.as_str(), "lkgp");
        assert_eq!(PriorityMode::Weighted.as_str(), "weighted");
        assert_eq!(PriorityMode::LeastUsed.as_str(), "least_used");
        assert_eq!(PriorityMode::P2c.as_str(), "p2c");
    }
}
