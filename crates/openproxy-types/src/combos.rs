use crate::config::CooldownMode;
use crate::ids::{AccountId, ComboId, ComboTargetId, ModelRowId, ProviderId};
use serde::{Deserialize, Serialize};

pub const MAX_SUB_COMBO_DEPTH: u32 = 5;

impl_string_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum Strategy {
        Priority => "priority",
        RoundRobin => "round_robin",
        Shuffle => "shuffle",
    }
    error: "strategy"
}

impl_string_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
    #[serde(rename_all = "snake_case")]
    pub enum PriorityMode {
        #[default]
        Strict => "strict",
        Lkgp => "lkgp",
        Weighted => "weighted",
        LeastUsed => "least_used",
        P2c => "p2c",
    }
    error: "priority_mode"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Combo {
    pub id: ComboId,
    pub name: String,
    pub strategy: Strategy,
    pub race_size: u8,
    #[serde(default)]
    pub preventive_rate_limit: bool,
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
    #[serde(default = "default_true")]
    pub active: bool,
    #[serde(default)]
    pub rate_limit_scope: crate::providers::RateLimitScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_mode: Option<crate::config::CooldownMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_base_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_max_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_factor: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComboTargetWithModel {
    pub id: ComboTargetId,
    pub combo_id: ComboId,
    pub provider_id: ProviderId,
    pub account_id: Option<AccountId>,
    pub model_row_id: Option<ModelRowId>,
    pub sub_combo_id: Option<ComboId>,
    pub sub_combo_name: Option<Box<str>>,
    pub model_id: Box<str>,
    pub model_display_name: Option<Box<str>>,
    pub priority_order: i32,
    #[serde(default = "default_target_weight")]
    pub weight: i32,
    #[serde(default)]
    pub in_cooldown: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<Box<str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_reason: Option<Box<str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i64>,
    #[serde(default = "default_true")]
    pub active: bool,
    #[serde(default = "default_true")]
    pub provider_active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_mode: Option<crate::config::CooldownMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_base_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_max_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_factor: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strategy_parse() {
        assert_eq!(Strategy::parse("priority"), Ok(Strategy::Priority));
        assert_eq!(Strategy::parse("round_robin"), Ok(Strategy::RoundRobin));
        assert_eq!(Strategy::parse("shuffle"), Ok(Strategy::Shuffle));
        assert_eq!(
            Strategy::parse("unknown"),
            Err("invalid strategy: unknown".to_string())
        );
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
