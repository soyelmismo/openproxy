//! Combos: ordered list of targets with a strategy. Priority or round-robin.
//! Each target references a (provider, model, optional account). Accounts can be rotated within a provider.

use crate::error::{CoreError, Result};
use crate::ids::{AccountId, ComboId, ComboTargetId, ModelRowId, ProviderId};
use rand::RngExt;
use rand::prelude::SliceRandom;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub mod crud;
pub mod load_balancing;
pub mod resolution;

pub use crud::*;
pub use load_balancing::*;
pub use resolution::*;

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
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "priority" => Ok(Self::Priority),
            "round_robin" => Ok(Self::RoundRobin),
            "shuffle" => Ok(Self::Shuffle),
            other => Err(CoreError::Validation(format!(
                "invalid strategy: {}",
                other
            ))),
        }
    }
}

/// Per-combo priority mode, layered on top of [`Strategy`].
///
/// The `Strategy` enum controls the *order in which targets are visited*
/// (Priority = listed order, RoundRobin = rotated, Shuffle = randomized).
/// The `PriorityMode` enum controls *how that order is computed when
/// the strategy is [`Strategy::Priority`]* — the legacy `strict` walk,
/// the LKGP "least-known-good-provider" preference, weighted random,
/// least-used, or power-of-two-choices.
///
/// For [`Strategy::RoundRobin`] and [`Strategy::Shuffle`], the
/// `priority_mode` is ignored (the strategy already pins the order).
///
/// The mode is stored as a nullable `priority_mode` TEXT column on
/// `combos`; `NULL` and `"strict"` mean the same thing (current
/// behavior). New combos default to `Strict`; existing rows that
/// pre-date migration 000035 read back as `Strict` via
/// [`PriorityMode::from_db`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PriorityMode {
    /// Walk `priority_order` ASC (current behavior). The default;
    /// what every pre-migration-000035 combo behaves as.
    #[default]
    Strict,
    /// Least-Known-Good-Provider: prefer the target whose most recent
    /// successful request is the newest. Falls back to `priority_order`
    /// for ties and never-tried targets. An `exploration_rate` chance
    /// of picking a random target instead keeps the routing from
    /// getting permanently stuck on a single target.
    Lkgp,
    /// Weighted random: each target's probability is proportional to
    /// its `weight` column (default 1).
    Weighted,
    /// Least-used: prefer the target with the fewest total requests
    /// in the recent window (`selection_window_secs`, default 3600).
    LeastUsed,
    /// Power of Two Choices: pick two random targets, choose the one
    /// with fewer recent failures.
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
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "strict" => Ok(Self::Strict),
            "lkgp" => Ok(Self::Lkgp),
            "weighted" => Ok(Self::Weighted),
            "least_used" => Ok(Self::LeastUsed),
            "p2c" => Ok(Self::P2c),
            other => Err(CoreError::Validation(format!(
                "invalid priority_mode: {}",
                other
            ))),
        }
    }
    /// Map the raw DB string back to the enum. `NULL` (i.e. a row
    /// created before migration 000035) is interpreted as `Strict`
    /// so legacy combos keep their pre-migration behavior.
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

/// Per-combo cooldown growth mode, layered on top of the global
/// `cooldown_secs` config.
///
/// `Flat` (the default / NULL) is the current behavior: a target
/// that fails with a retryable error is parked for `cooldown_secs`
/// every time, regardless of how many times it has failed in a row.
///
/// `Exponential` grows the cooldown with `failure_count`:
/// `cooldown_until = now + min(base_secs * factor^(failure_count-1), max_secs)`.
/// A target that keeps flapping gets parked for progressively longer
/// windows, up to `max_secs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CooldownMode {
    #[default]
    Flat,
    Exponential,
}

impl CooldownMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Flat => "flat",
            Self::Exponential => "exponential",
        }
    }
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "flat" => Ok(Self::Flat),
            "exponential" => Ok(Self::Exponential),
            other => Err(CoreError::Validation(format!(
                "invalid cooldown_mode: {}",
                other
            ))),
        }
    }
    /// Map the raw DB string back to the enum. `NULL` (i.e. a row
    /// created before migration 000035) is interpreted as `Flat`
    /// so legacy combos keep their pre-migration behavior.
    pub fn from_db(s: Option<&str>) -> Self {
        match s {
            Some("exponential") => Self::Exponential,
            _ => Self::Flat,
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
    /// Operator-set context window override. `None` = auto-compute
    /// (minimum across all targets, including sub-combos recursively).
    /// `Some(n)` = use `n` as the reported context window in /v1/models.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<i64>,
    /// Priority mode for [`Strategy::Priority`]. `Strict` (the default)
    /// is the legacy `priority_order ASC` walk; the other variants
    /// change *how* the order is computed. See [`PriorityMode`].
    /// Ignored for [`Strategy::RoundRobin`] and [`Strategy::Shuffle`].
    #[serde(default)]
    pub priority_mode: PriorityMode,
    /// Cooldown growth mode. `Flat` (the default) parks a failed
    /// target for `cooldown_base_secs` every time; `Exponential`
    /// grows the window with `failure_count`. See [`CooldownMode`].
    #[serde(default)]
    pub cooldown_mode: CooldownMode,
    /// Per-combo override for the cooldown base. `None` = use the
    /// global `[cooldown] cooldown_secs` / `[cooldown] base_secs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_base_secs: Option<u64>,
    /// Per-combo override for the cooldown cap. `None` = use the
    /// global `[cooldown] max_secs` (default 3600). Only meaningful
    /// when `cooldown_mode = Exponential`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_max_secs: Option<u64>,
    /// Per-combo override for the exponential growth factor. `None`
    /// = use the global `[cooldown] factor` (default 2). Only
    /// meaningful when `cooldown_mode = Exponential`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_factor: Option<u32>,
    /// LKGP exploration rate (0.0–1.0). Probability of picking a
    /// random target instead of the best-known one. `None` = use
    /// the default 0.1 (10%). Only meaningful when
    /// `priority_mode = Lkgp`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lkgp_exploration_rate: Option<f64>,
    /// Selection window for `least_used` / `p2c` modes: how far back
    /// the in-memory registry looks at usage data. `None` = use the
    /// default 3600 (1 hour).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_window_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComboTarget {
    pub id: ComboTargetId,
    pub combo_id: ComboId,
    pub provider_id: ProviderId,
    pub account_id: Option<AccountId>, // None = rotate among healthy accounts of this provider
    /// `Some(_)` for a flat (model) target; `None` when this target
    /// is a sub-combo (i.e. `sub_combo_id` is set). The XOR between
    /// `model_row_id` and `sub_combo_id` is enforced in
    /// [`add_target`].
    pub model_row_id: Option<ModelRowId>,
    /// `Some(_)` for a sub-combo target; `None` for a flat (model)
    /// target. Sub-combo targets are flattened by
    /// [`resolve_combo_to_targets`] before the pipeline iterates the
    /// resolved list — they never reach `execute_single` directly.
    pub sub_combo_id: Option<ComboId>,
    pub priority_order: i32,
    /// Per-target weight for the `weighted` priority mode. The column
    /// is `INTEGER NOT NULL DEFAULT 1` (migration 000035); existing
    /// rows that pre-date the migration read back as `1`.
    #[serde(default = "default_target_weight")]
    pub weight: i32,
    #[serde(default)]
    pub rate_limit_scope: crate::providers::RateLimitScope,
}

/// Default weight for `ComboTarget::weight`. Used by serde when the
/// JSON shape omits the field (e.g. an older dashboard POSTing to a
/// pre-migration API) and by the test helpers that build rows by
/// hand. The DB column itself is `NOT NULL DEFAULT 1`, so the only
/// path that yields `0` is a caller that explicitly writes `0`.
fn default_target_weight() -> i32 {
    1
}

/// Combo target enriched with the model's display metadata. Used by the
/// admin API so the dashboard can render a human-readable model id
/// (`model_id` = upstream id like `anthropic/claude-3.5-sonnet` and
/// `model_display_name` = the row's `display_name`) without doing a
/// per-row roundtrip to `GET /admin/models`.
///
/// The pipeline's hot path still uses [`ComboTarget`] — `expand_account_rotation`
/// and `resolve_target_order` work on the slim shape — so the enriched
/// variant is intentionally a separate type.
///
/// The three trailing `in_cooldown` / `cooldown_until` / `cooldown_reason`
/// fields are populated by a `LEFT JOIN` against `target_cooldowns` and
/// let the dashboard render the "⏸ cooldown" badge inline with each
/// row. The fields are `Option`/`bool` so adding the cooldown feature
/// to existing rows in flight is a no-op on the JSON shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComboTargetWithModel {
    pub id: ComboTargetId,
    pub combo_id: ComboId,
    pub provider_id: ProviderId,
    pub account_id: Option<AccountId>,
    /// `Some(_)` for a flat (model) target; `None` for a sub-combo
    /// target. See [`ComboTarget::model_row_id`].
    pub model_row_id: Option<ModelRowId>,
    /// `Some(_)` for a sub-combo target; `None` for a flat target.
    /// See [`ComboTarget::sub_combo_id`].
    pub sub_combo_id: Option<ComboId>,
    /// Upstream sub-combo name (the row's `combos.name`) for sub-combo
    /// targets. `None` for flat targets.
    pub sub_combo_name: Option<String>,
    /// Upstream model id from `models.model_id` (e.g. `"anthropic/claude-3.5-sonnet"`).
    /// Empty string for sub-combo targets or if the model row was
    /// somehow deleted (FK cascade should prevent the latter, but we
    /// fall back to `""` to keep the JSON shape stable).
    pub model_id: String,
    /// Optional human-readable name from `models.display_name`. `None`
    /// for sub-combo targets, for rows created before display names
    /// were supported, or for upstream providers that don't expose
    /// one.
    pub model_display_name: Option<String>,
    pub priority_order: i32,
    /// Per-target weight for the `weighted` priority mode. Mirrors
    /// [`ComboTarget::weight`]. The dashboard renders this as an
    /// editable input next to each row.
    #[serde(default = "default_target_weight")]
    pub weight: i32,
    /// `true` when this target has an active row in `target_cooldowns`
    /// (`cooldown_until > now`). Always `false` for sub-combo targets
    /// — cooldowns attach to the *child* targets after flattening, not
    /// to the indirection row itself.
    #[serde(default)]
    pub in_cooldown: bool,
    /// ISO 8601 UTC of the cooldown expiry. `None` when not in
    /// cooldown. Surfaced so the dashboard can show a relative-time
    /// "back in 30s" hint without re-deriving the timestamp client-
    /// side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<String>,
    /// Last error string that fired the cooldown. `None` when not in
    /// cooldown. Useful as a tooltip so the operator doesn't have to
    /// open the usage errors view to find out *why* a target is
    /// parked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_reason: Option<String>,
    /// Model's context_length (from `models.context_length`). `None`
    /// for sub-combo targets or if the model row has no metadata.
    /// Surfaced so the dashboard can show the context window per
    /// target in the combo editor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<i64>,
    /// Model's max output tokens (from `models.max_output_tokens`).
    /// `None` for sub-combo targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i64>,
    /// `true` when this target's provider is active (`providers.active = 1`).
    /// `false` when the provider has been deactivated. The dashboard
    /// shows a "provider inactive" badge on these rows so the operator
    /// knows the target won't be used for routing — but the row is still
    /// visible (and reorderable) so the operator can manage it.
    ///
    /// CRITICAL: `list_targets_with_model` returns targets with BOTH
    /// active and inactive providers. The routing path (`list_targets`)
    /// still filters by `p.active = 1` — only active targets are used
    /// for actual request routing. The dashboard view is a superset so
    /// the operator can see and manage all targets, including the
    /// inactive ones. This prevents the reorder bug where the GET
    /// returned a filtered list (missing inactive targets) but the
    /// reorder validation operated on the full list (including inactive
    /// targets), causing a mismatch and a 400 error.
    #[serde(default = "default_true")]
    pub provider_active: bool,
}

fn default_true() -> bool {
    true
}
