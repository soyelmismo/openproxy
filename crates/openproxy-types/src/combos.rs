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
        Decision => "decision",
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_timeout_ms: Option<u64>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Default for ComboTarget {
    fn default() -> Self {
        Self {
            id: ComboTargetId(0),
            combo_id: ComboId(0),
            provider_id: ProviderId::new(""),
            account_id: None,
            model_row_id: None,
            sub_combo_id: None,
            priority_order: 0,
            weight: 1,
            active: true,
            rate_limit_scope: crate::providers::RateLimitScope::Account,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            thinking_effort: None,
            description: None,
        }
    }
}

impl Default for Combo {
    fn default() -> Self {
        Self {
            id: ComboId(0),
            name: String::new(),
            strategy: Strategy::Priority,
            race_size: 1,
            preventive_rate_limit: false,
            created_at: String::new(),
            context_window: None,
            priority_mode: PriorityMode::Strict,
            cooldown_mode: CooldownMode::Flat,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            lkgp_exploration_rate: None,
            selection_window_secs: None,
            decision_model: None,
            decision_timeout_ms: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TargetExecutionId {
    pub target_id: ComboTargetId,
    pub account_id: Option<AccountId>,
}

impl TargetExecutionId {
    #[inline]
    pub const fn new(target_id: ComboTargetId, account_id: Option<AccountId>) -> Self {
        Self {
            target_id,
            account_id,
        }
    }

    #[inline]
    pub fn from_target(target: &ComboTarget) -> Self {
        Self {
            target_id: target.id,
            account_id: target.account_id,
        }
    }

    #[inline]
    pub fn matches_target(&self, target: &ComboTarget) -> bool {
        self.target_id == target.id
            && (self.account_id.is_none() || self.account_id == target.account_id)
    }
}

impl From<ComboTargetId> for TargetExecutionId {
    #[inline]
    fn from(id: ComboTargetId) -> Self {
        Self {
            target_id: id,
            account_id: None,
        }
    }
}

impl From<&ComboTarget> for TargetExecutionId {
    #[inline]
    fn from(target: &ComboTarget) -> Self {
        Self::from_target(target)
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_effort: Option<Box<str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<Box<str>>,
}

/// Input for adding a target to a combo.
///
/// Exactly one of `model_row_id` (a flat target) or `sub_combo_id` (a combo-in-combo
/// target) must be provided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddTargetInput {
    pub combo_id: ComboId,
    pub provider_id: ProviderId,
    pub account_id: Option<AccountId>,
    pub model_row_id: Option<ModelRowId>,
    pub sub_combo_id: Option<ComboId>,
    pub priority_order: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Combo {
    /// Returns `true` if cooldown is disabled at the combo level.
    #[inline]
    pub fn is_cooldown_disabled(&self) -> bool {
        self.cooldown_mode == CooldownMode::None || self.cooldown_base_secs == Some(0)
    }
}

impl ComboTarget {
    /// Returns `true` if this target explicitly forces cooldown to disabled.
    #[inline]
    pub fn is_cooldown_forced_disabled(&self) -> bool {
        self.cooldown_mode == Some(crate::config::CooldownMode::None)
            || self.cooldown_base_secs == Some(0)
    }

    /// Returns `true` if this target's cooldown is disabled either explicitly
    /// on the target or inherited from the combo.
    #[inline]
    pub fn is_cooldown_disabled(&self, combo: &Combo) -> bool {
        self.is_cooldown_forced_disabled()
            || (self.cooldown_mode.is_none() && combo.is_cooldown_disabled())
    }
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
        assert_eq!(PriorityMode::parse("decision"), Ok(PriorityMode::Decision));
        assert!(PriorityMode::parse("unknown").is_err());
    }

    #[test]
    fn test_priority_mode_as_str() {
        assert_eq!(PriorityMode::Strict.as_str(), "strict");
        assert_eq!(PriorityMode::Lkgp.as_str(), "lkgp");
        assert_eq!(PriorityMode::Weighted.as_str(), "weighted");
        assert_eq!(PriorityMode::LeastUsed.as_str(), "least_used");
        assert_eq!(PriorityMode::P2c.as_str(), "p2c");
        assert_eq!(PriorityMode::Decision.as_str(), "decision");
    }

    #[test]
    fn test_combo_target_cooldown_disabled() {
        let mut target = ComboTarget {
            id: ComboTargetId(1),
            combo_id: ComboId(1),
            provider_id: crate::ProviderId("openai".into()),
            account_id: None,
            model_row_id: None,
            sub_combo_id: None,
            priority_order: 1,
            weight: 1,
            active: true,
            rate_limit_scope: crate::providers::RateLimitScope::Account,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            thinking_effort: None,
            description: None,
        };

        let mut combo = Combo {
            id: ComboId(1),
            name: "test".into(),
            strategy: Strategy::Priority,
            race_size: 1,
            preventive_rate_limit: true,
            created_at: "now".into(),
            context_window: None,
            priority_mode: PriorityMode::Strict,
            cooldown_mode: CooldownMode::Flat,
            cooldown_base_secs: Some(60),
            cooldown_max_secs: None,
            cooldown_factor: None,
            lkgp_exploration_rate: None,
            selection_window_secs: None,
            decision_model: None,
            decision_timeout_ms: None,
        };

        // Default: inherits combo (Flat 60s) -> not disabled
        assert!(!combo.is_cooldown_disabled());
        assert!(!target.is_cooldown_forced_disabled());
        assert!(!target.is_cooldown_disabled(&combo));

        // Forced disabled on target via CooldownMode::None
        target.cooldown_mode = Some(CooldownMode::None);
        assert!(target.is_cooldown_forced_disabled());
        assert!(target.is_cooldown_disabled(&combo));

        // Reset mode, force disabled via base_secs = 0
        target.cooldown_mode = None;
        target.cooldown_base_secs = Some(0);
        assert!(target.is_cooldown_forced_disabled());
        assert!(target.is_cooldown_disabled(&combo));

        // Inherits from combo when combo has CooldownMode::None
        target.cooldown_base_secs = None;
        combo.cooldown_mode = CooldownMode::None;
        assert!(combo.is_cooldown_disabled());
        assert!(!target.is_cooldown_forced_disabled());
        assert!(target.is_cooldown_disabled(&combo));
    }
}
