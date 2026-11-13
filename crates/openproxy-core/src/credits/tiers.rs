//! Credit tiers: reusable key-shaping presets referenced by packages.
//!
//! A tier bundles what a buyer's key is *allowed to reach* (model/provider/
//! combo allowlists) plus the quota caps that bound the spend. Packages point
//! at a tier, so editing the tier reshapes every key sold through it without
//! touching the packages one by one.
//!
//! Money invariant: when a tier carries an allowlist, the key is forced into
//! `only_whitelisted = 1`. Leveraged credit must never reach a model outside
//! the tier — an allowlist that is not enforced is a suggestion, not a gate.

use openproxy_types::ids::ApiKeyId;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// Key-shaping payload of a tier. `None` allowlist = leave the key as-is.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TierConfig {
    pub weekly_usd_cap: Option<f64>,
    pub five_hour_usd_cap: Option<f64>,
    pub allowed_models: Option<Vec<String>>,
    pub allowed_providers: Option<Vec<String>>,
    pub allowed_combos: Option<Vec<String>>,
    pub only_whitelisted: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreditTier {
    pub id: i64,
    pub name: String,
    pub config: TierConfig,
    pub active: bool,
    /// Plan commerce (the tier IS the product since 000115): what the buyer
    /// pays. `None` = draft, not listed for sale.
    pub price_usd: Option<f64>,
    /// Balance credited on payment. `None` = 1:1 (credit == price).
    pub credit_usd: Option<f64>,
    /// Plan lifetime in days; the balance lot expires after this many days.
    /// The key itself keeps living (re-subscription is the buyer's choice).
    pub ttl_days: i64,
    /// How many live keys inherited this tier (the single-source-of-truth
    /// propagation surface: an edit re-shapes exactly this many keys).
    pub key_count: i64,
}

impl CreditTier {
    /// Listed for sale: active and with a price the buyer would pay.
    pub fn sellable(&self) -> bool {
        self.active && self.price_usd.is_some()
    }
}

const TIER_COLS: &str = "t.id, t.name, t.models_json, t.providers_json, t.combos_json, \
                         t.only_whitelisted, t.weekly_usd_cap, \
                         t.five_hour_usd_cap, t.active, t.price_usd, t.credit_usd, t.ttl_days";

fn json_vec(raw: Option<String>) -> Option<Vec<String>> {
    raw.and_then(|j| serde_json::from_str(&j).ok())
}

fn row_to_tier(row: &rusqlite::Row<'_>) -> rusqlite::Result<CreditTier> {
    Ok(CreditTier {
        id: row.get(0)?,
        name: row.get(1)?,
        config: TierConfig {
            allowed_models: json_vec(row.get(2)?),
            allowed_providers: json_vec(row.get(3)?),
            allowed_combos: json_vec(row.get(4)?),
            only_whitelisted: row.get::<_, Option<i64>>(5)?.map(|v| v != 0),
            weekly_usd_cap: row.get(6)?,
            five_hour_usd_cap: row.get(7)?,
        },
        active: row.get::<_, i64>(8)? != 0,
        price_usd: row.get(9)?,
        credit_usd: row.get(10)?,
        ttl_days: row.get::<_, Option<i64>>(11)?.unwrap_or(30),
        key_count: row.get(12).unwrap_or(0),
    })
}

/// Every tier, with the number of live keys inheriting it.
///
/// # Errors
/// Propagates DB failures.
pub fn list(conn: &Connection) -> openproxy_types::Result<Vec<CreditTier>> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {TIER_COLS}, \
             (SELECT COUNT(*) FROM api_keys k WHERE k.tier_id = t.id) \
             FROM credit_tiers t ORDER BY t.active DESC, t.name ASC"
        ))
        .map_err(openproxy_db::error::map_db_error)?;
    let rows = stmt
        .query_map([], row_to_tier)
        .map_err(openproxy_db::error::map_db_error)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(openproxy_db::error::map_db_error)
}

/// Fetch one tier.
///
/// # Errors
/// Propagates DB failures.
pub fn get(conn: &Connection, id: i64) -> openproxy_types::Result<Option<CreditTier>> {
    conn.query_row(
        &format!(
            "SELECT {TIER_COLS}, \
             (SELECT COUNT(*) FROM api_keys k WHERE k.tier_id = t.id) \
             FROM credit_tiers t WHERE t.id = ?1"
        ),
        params![id],
        row_to_tier,
    )
    .optional()
    .map_err(openproxy_db::error::map_db_error)
}

fn validate_name(name: &str) -> openproxy_types::Result<String> {
    let name = name.trim().to_string();
    if name.is_empty() || name.len() > 64 {
        return Err(openproxy_types::CoreError::Validation(
            "tier name must be 1..=64 characters".into(),
        ));
    }
    Ok(name)
}

/// Plan commerce sanity (000115): a listed price and credit must be positive,
/// the lifetime must be at least one day. `None` price = draft, `None` credit
/// = 1:1.
fn validate_plan(
    price_usd: Option<f64>,
    credit_usd: Option<f64>,
    ttl_days: Option<i64>,
) -> openproxy_types::Result<(Option<f64>, Option<f64>, i64)> {
    if let Some(p) = price_usd
        && crate::credits::usd_to_units(p)
            .and_then(|units| units.checked_add(9_999))
            .is_none()
    {
        return Err(openproxy_types::CoreError::Validation(
            "price_usd must be a positive number".into(),
        ));
    }
    if let Some(c) = credit_usd
        && crate::credits::usd_to_units(c).is_none()
    {
        return Err(openproxy_types::CoreError::Validation(
            "credit_usd must be a positive number".into(),
        ));
    }
    let ttl = ttl_days.unwrap_or(30);
    if !(1..=36_500).contains(&ttl) {
        return Err(openproxy_types::CoreError::Validation(
            "ttl_days must be between 1 and 36500".into(),
        ));
    }
    Ok((price_usd, credit_usd, ttl))
}

fn sanitize(config: &TierConfig) -> TierConfig {
    fn cap(v: Option<f64>) -> Option<f64> {
        match v {
            Some(x) if x.is_finite() && x >= 0.0 => Some(x),
            _ => None,
        }
    }
    TierConfig {
        weekly_usd_cap: cap(config.weekly_usd_cap),
        five_hour_usd_cap: cap(config.five_hour_usd_cap),
        allowed_models: config.allowed_models.clone(),
        allowed_providers: config.allowed_providers.clone(),
        allowed_combos: config.allowed_combos.clone(),
        only_whitelisted: config.only_whitelisted,
    }
}

/// The commerce half of a tier (000115): the plan the buyer sees.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PlanCommerce {
    pub price_usd: Option<f64>,
    pub credit_usd: Option<f64>,
    pub ttl_days: i64,
}

/// Create a tier. Duplicate names are rejected by the UNIQUE index.
///
/// # Errors
/// Validation and DB failures.
pub fn create(
    conn: &Connection,
    name: &str,
    config: &TierConfig,
    plan: &PlanCommerce,
) -> openproxy_types::Result<CreditTier> {
    let name = validate_name(name)?;
    let cfg = sanitize(config);
    // The admin UI sends ONE merged selection; combo entries (`combo:` prefix
    // or names matching a live combo row) must land in combos_json or the
    // routing gate never matches them. Store a canonical row.
    let (models, combos) = partition_names(
        conn,
        cfg.allowed_models.as_ref(),
        cfg.allowed_combos.as_ref(),
    )?;
    let (price_usd, credit_usd, ttl_days) =
        validate_plan(plan.price_usd, plan.credit_usd, Some(plan.ttl_days))?;
    conn.execute(
        "INSERT INTO credit_tiers (name, models_json, providers_json, combos_json, \
             only_whitelisted, weekly_usd_cap, five_hour_usd_cap, \
             price_usd, credit_usd, ttl_days) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            name,
            models
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_default()),
            cfg.allowed_providers
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_default()),
            combos
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_default()),
            cfg.only_whitelisted.map(i64::from),
            cfg.weekly_usd_cap,
            cfg.five_hour_usd_cap,
            price_usd,
            credit_usd,
            ttl_days,
        ],
    )
    .map_err(openproxy_db::error::map_db_error)?;
    let id = conn.last_insert_rowid();
    get(conn, id)?
        .ok_or_else(|| openproxy_types::CoreError::Internal("tier vanished after insert".into()))
}

/// Update a tier. `config: Some` replaces the whole payload (the admin UI
/// always sends the complete object); package references stay intact.
///
/// # Errors
/// Validation and DB failures.
pub fn update(
    conn: &Connection,
    id: i64,
    name: Option<&str>,
    config: Option<&TierConfig>,
    active: Option<bool>,
    plan: Option<&PlanCommerce>,
) -> openproxy_types::Result<()> {
    super::transaction(conn, |conn| {
        update_tier(conn, id, name, config, active, plan)
    })
}

fn update_tier(
    conn: &Connection,
    id: i64,
    name: Option<&str>,
    config: Option<&TierConfig>,
    active: Option<bool>,
    plan: Option<&PlanCommerce>,
) -> openproxy_types::Result<()> {
    if let Some(n) = name {
        let name = validate_name(n)?;
        conn.execute(
            "UPDATE credit_tiers SET name = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![name, id],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    }
    if let Some(c) = config {
        let cfg = sanitize(c);
        // Same canonical partition as create: combo names into combos_json,
        // model ids into models_json (see the create path for the why).
        let (models, combos) = partition_names(
            conn,
            cfg.allowed_models.as_ref(),
            cfg.allowed_combos.as_ref(),
        )?;
        conn.execute(
            "UPDATE credit_tiers SET models_json = ?1, providers_json = ?2, combos_json = ?3, \
                 only_whitelisted = ?4, weekly_usd_cap = ?5, \
                 five_hour_usd_cap = ?6, updated_at = datetime('now') WHERE id = ?7",
            params![
                models
                    .as_ref()
                    .map(|v| serde_json::to_string(v).unwrap_or_default()),
                cfg.allowed_providers
                    .as_ref()
                    .map(|v| serde_json::to_string(v).unwrap_or_default()),
                combos
                    .as_ref()
                    .map(|v| serde_json::to_string(v).unwrap_or_default()),
                cfg.only_whitelisted.map(i64::from),
                cfg.weekly_usd_cap,
                cfg.five_hour_usd_cap,
                id,
            ],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    }
    if let Some(a) = active {
        conn.execute(
            "UPDATE credit_tiers SET active = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![i64::from(a), id],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    }
    // The plan half (price/credit/ttl) updates separately from the access
    // contract: a sale-off toggle (active) must not need the whole payload.
    if let Some(p) = plan {
        let (price, credit, ttl) = validate_plan(p.price_usd, p.credit_usd, Some(p.ttl_days))?;
        conn.execute(
            "UPDATE credit_tiers SET price_usd = ?1, credit_usd = ?2, ttl_days = ?3, \
                 updated_at = datetime('now') WHERE id = ?4",
            params![price, credit, ttl, id],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    }
    // Single source of truth: a config edit re-shapes every key linked to
    // this tier, in the same transaction (Miguel, 2026-11-12). Active
    // toggles only affect NEW purchases, not already-linked keys.
    if config.is_some() {
        let keys = propagate_tier_to_keys(conn, id)?;
        if keys > 0 {
            tracing::info!(tier_id = id, keys, "tier propagated to linked keys");
        }
    }
    Ok(())
}

/// Delete a tier by id (no-op when it does not exist). Keys inherit their
/// contract from the tier at apply time, so a tier still linked to live keys
/// is refused by the caller (deleting one would orphan them).
///
/// # Errors
/// - [`openproxy_types::CoreError::Validation`] when keys still use it.
/// - Propagates DB failures.
pub fn delete(conn: &Connection, id: i64) -> openproxy_types::Result<()> {
    super::transaction(conn, |conn| delete_tier(conn, id))
}

fn delete_tier(conn: &Connection, id: i64) -> openproxy_types::Result<()> {
    if crate::credits::tiers::get(conn, id)?.is_none() {
        return Ok(());
    }
    let used: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM api_keys WHERE tier_id = ?1",
            params![id],
            |r| r.get(0),
        )
        .map_err(openproxy_db::error::map_db_error)?;
    let pending: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM credit_orders WHERE tier_id = ?1 \
         AND status IN ('pending', 'expired') AND datetime(expires_at, '+600 seconds') >= datetime('now'))",
        [id], |row| row.get(0),
    ).map_err(openproxy_db::error::map_db_error)?;
    if used > 0 || pending {
        return Err(openproxy_types::CoreError::Validation(format!(
            "tier is used by {used} key(s): deactivate it instead"
        )));
    }
    conn.execute("DELETE FROM credit_tiers WHERE id = ?1", params![id])
        .map_err(openproxy_db::error::map_db_error)?;
    Ok(())
}

/// Apply a tier to a key: contract of the purchase.
///
/// - Quota caps are written verbatim (a `None` cap means no limit).
/// - Allowlists overwrite when present, `None` leaves the key untouched.
/// - An allowlist forces `only_whitelisted = 1` unless the tier says
///   otherwise, so the gate is real and not advisory.
///
/// # Errors
/// Propagates DB failures.
pub fn apply_to_key(
    conn: &Connection,
    key_id: ApiKeyId,
    config: &TierConfig,
) -> openproxy_types::Result<()> {
    let caps = crate::api_key_quotas::QuotaCaps {
        weekly_usd_cap: config.weekly_usd_cap,
        five_hour_usd_cap: config.five_hour_usd_cap,
    };
    crate::api_key_quotas::upsert(conn, key_id, &caps)?;

    // Combo entries may ride in allowed_models (merged admin selection);
    // partition before writing, and store combo ids — the shape the routing
    // gate consumes (`is_combo_allowed`).
    let (models, combos) = partition_names(
        conn,
        config.allowed_models.as_ref(),
        config.allowed_combos.as_ref(),
    )?;
    if let Some(models) = &models {
        conn.execute(
            "UPDATE api_keys SET allowed_models_json = ?1 WHERE id = ?2",
            params![serde_json::to_string(models).unwrap_or_default(), key_id.0],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    }
    if let Some(providers) = &config.allowed_providers {
        conn.execute(
            "UPDATE api_keys SET allowed_providers_json = ?1 WHERE id = ?2",
            params![
                serde_json::to_string(providers).unwrap_or_default(),
                key_id.0
            ],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    }
    if let Some(combos) = &combos {
        let ids = resolve_combo_ids(conn, combos)?;
        conn.execute(
            "UPDATE api_keys SET allowed_combos_json = ?1 WHERE id = ?2",
            params![ids, key_id.0],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    }
    let allowlist_present = models.is_some() || combos.is_some();
    let forced_gate = config
        .only_whitelisted
        .or(if allowlist_present { Some(true) } else { None });
    if let Some(only) = forced_gate {
        conn.execute(
            "UPDATE api_keys SET only_whitelisted = ?1 WHERE id = ?2",
            params![i64::from(only), key_id.0],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    }
    Ok(())
}

pub use super::plan_grants::grant_plan;

// ── propagation & key-shaping (moved from packages.rs: the tier domain) ──

/// Resolve combo names to the numeric combo ids the routing layer consumes
/// (`api_keys.allowed_combos_json` feeds `is_combo_allowed`). Names without a
/// live combo row are dropped: a stale entry must never lock a key out of
/// everything else.
pub(crate) fn resolve_combo_ids(
    conn: &Connection,
    names: &[String],
) -> openproxy_types::Result<String> {
    if names.is_empty() {
        return Ok("[]".into());
    }
    let mut ids: Vec<i64> = Vec::new();
    let mut stmt = conn
        .prepare("SELECT id FROM combos WHERE name = ?1 LIMIT 1")
        .map_err(openproxy_db::error::map_db_error)?;
    for name in names {
        if let Ok(id) = stmt.query_row(params![name], |r| r.get::<_, i64>(0)) {
            ids.push(id);
        }
    }
    ids.sort_unstable();
    ids.dedup();
    Ok(serde_json::to_string(&ids).unwrap_or_else(|_| "[]".into()))
}

/// Partition a tier's combined allowlist into the two wire shapes the routing
/// layer consumes: plain entries are model ids (`allowed_models_json`, what
/// `is_model_allowed` checks) and `combo:<name>` entries (or bare combo names)
/// resolve to numeric combo ids (`allowed_combos_json`, what `is_combo_allowed`
/// checks). `None` inputs yield `None` payloads (leave the column untouched).
///
/// # Errors
/// DB failures while resolving combo ids.
fn allowlist_payload(
    conn: &Connection,
    models: Option<&Vec<String>>,
    combos: Option<&Vec<String>>,
) -> openproxy_types::Result<(Option<String>, Option<String>)> {
    let mut model_ids: Vec<String> = Vec::new();
    let mut combo_names: Vec<String> = Vec::new();
    if let Some(ms) = models {
        for e in ms {
            if let Some(name) = e.strip_prefix("combo:") {
                if !name.is_empty() {
                    combo_names.push(name.to_string());
                }
            } else {
                model_ids.push(e.clone());
            }
        }
    }
    if let Some(cs) = combos {
        for e in cs {
            let name = e.strip_prefix("combo:").unwrap_or(e);
            if !name.is_empty() {
                combo_names.push(name.to_string());
            }
        }
    }
    let models_json =
        models.map(|_| serde_json::to_string(&model_ids).unwrap_or_else(|_| "[]".into()));
    let combos_json = if models.is_some() || combos.is_some() {
        Some(resolve_combo_ids(conn, &combo_names)?)
    } else {
        None
    };
    Ok((models_json, combos_json))
}

/// Split a combined allowlist into (model ids, combo names) at NAME level.
/// The admin UI sends one merged selection; entries with the `combo:` prefix
/// — and bare names matching a live combo row — are combo names, everything
/// else stays a model id. Used to store a canonical tier row: the propagation
/// layer resolves these names to numeric ids per key.
///
/// # Errors
/// Propagates DB failures.
type PartitionedNames = (Option<Vec<String>>, Option<Vec<String>>);

pub(crate) fn partition_names(
    conn: &Connection,
    models: Option<&Vec<String>>,
    combos: Option<&Vec<String>>,
) -> openproxy_types::Result<PartitionedNames> {
    if let (None, None) = (models, combos) {
        return Ok((None, None));
    }
    let mut model_ids: Vec<String> = Vec::new();
    let mut combo_names: Vec<String> = Vec::new();
    let is_combo_name = |conn: &Connection, entry: &str| -> openproxy_types::Result<bool> {
        if let Some(name) = entry.strip_prefix("combo:") {
            return Ok(!name.is_empty());
        }
        let mut stmt = conn
            .prepare("SELECT 1 FROM combos WHERE name = ?1")
            .map_err(openproxy_db::error::map_db_error)?;
        let found = stmt
            .query_row(params![entry], |_| Ok(()))
            .optional()
            .map_err(openproxy_db::error::map_db_error)?;
        Ok(found.is_some())
    };
    if let Some(ms) = models {
        for e in ms {
            if is_combo_name(conn, e)? {
                if let Some(name) = e.strip_prefix("combo:") {
                    combo_names.push(name.to_string());
                } else {
                    combo_names.push(e.clone());
                }
            } else {
                model_ids.push(e.clone());
            }
        }
    }
    if let Some(cs) = combos {
        for e in cs {
            if let Some(name) = e.strip_prefix("combo:") {
                if !name.is_empty() {
                    combo_names.push(name.to_string());
                }
            } else {
                combo_names.push(e.clone());
            }
        }
    }
    combo_names.sort();
    combo_names.dedup();
    let models_out = models.map(|_| model_ids);
    let combos_out = if models.is_some() || combos.is_some() || !combo_names.is_empty() {
        Some(combo_names)
    } else {
        None
    };
    Ok((models_out, combos_out))
}

/// Persist which tier a leveraged key inherited. This link (not the copied
/// snapshot) is what makes the tier the single source of truth: edits to the
/// tier re-shape every key pointing at it.
///
/// # Errors
/// Propagates DB failures.
pub fn set_key_tier(
    conn: &Connection,
    key_id: ApiKeyId,
    tier_id: Option<i64>,
) -> openproxy_types::Result<()> {
    conn.execute(
        "UPDATE api_keys SET tier_id = ?1 WHERE id = ?2",
        params![tier_id, key_id.0],
    )
    .map_err(openproxy_db::error::map_db_error)?;
    Ok(())
}

/// The tier's own shaping, independent of any package: caps and allowlists as
/// the tier currently defines them. This is the LIVE contract propagation
/// applies — unlike the purchase path, caps are replaced, never or-merged, so
/// removing a cap in the tier removes it from the keys.
#[must_use]
pub fn tier_own_config(tier: &CreditTier) -> TierConfig {
    let t = &tier.config;
    // Propagation always writes a definite strict-mode flag: an unrestricted
    // tier must CLEAR only_whitelisted (a stale 1 with cleared allowlists
    // denies every request).
    let gate = Some(t.only_whitelisted.unwrap_or_else(|| {
        t.allowed_models.is_some() || t.allowed_providers.is_some() || t.allowed_combos.is_some()
    }));
    TierConfig {
        weekly_usd_cap: t.weekly_usd_cap,
        five_hour_usd_cap: t.five_hour_usd_cap,
        allowed_models: t.allowed_models.clone(),
        allowed_providers: t.allowed_providers.clone(),
        allowed_combos: t.allowed_combos.clone(),
        only_whitelisted: gate,
    }
}

/// Re-shape every key linked to `tier_id` with the tier's CURRENT config.
/// Called after a tier edit: the tier is the single source of truth and the
/// keys inherit the change immediately (Miguel, 2026-11-12). Quota caps
/// are replaced, allowlists are overwritten wholesale. Returns the number of
/// keys touched.
///
/// # Errors
/// Propagates DB failures.
pub fn propagate_tier_to_keys(conn: &Connection, tier_id: i64) -> openproxy_types::Result<usize> {
    let Some(tier) = get(conn, tier_id)? else {
        return Ok(0);
    };
    let config = tier_own_config(&tier);
    let key_ids: Vec<ApiKeyId> = {
        let mut stmt = conn
            .prepare("SELECT id FROM api_keys WHERE tier_id = ?1 AND is_active = 1")
            .map_err(openproxy_db::error::map_db_error)?;
        let rows = stmt
            .query_map(params![tier_id], |row| row.get::<_, i64>(0))
            .map_err(openproxy_db::error::map_db_error)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(openproxy_db::error::map_db_error)?
            .into_iter()
            .map(ApiKeyId)
            .collect()
    };
    for kid in &key_ids {
        apply_to_key_forced(conn, *kid, &config)?;
    }
    Ok(key_ids.len())
}

/// Like [`apply_to_key`] but authoritative: quota caps are SET (a `None` clears
/// the previous cap) and allowlists are written even when the tier no longer
/// constrains them. Used by tier propagation, where the tier is the truth.
///
/// # Errors
/// Propagates DB failures.
pub fn apply_to_key_forced(
    conn: &Connection,
    key_id: ApiKeyId,
    config: &TierConfig,
) -> openproxy_types::Result<()> {
    let caps = crate::api_key_quotas::QuotaCaps {
        weekly_usd_cap: config.weekly_usd_cap,
        five_hour_usd_cap: config.five_hour_usd_cap,
    };
    crate::api_key_quotas::upsert(conn, key_id, &caps)?;
    let (models_json, combos_json) = allowlist_payload(
        conn,
        config.allowed_models.as_ref(),
        config.allowed_combos.as_ref(),
    )?;
    if let Some(payload) = models_json {
        conn.execute(
            "UPDATE api_keys SET allowed_models_json = ?1 WHERE id = ?2",
            params![payload, key_id.0],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    } else {
        // Unrestricted tier: the tier owns the truth, so the inherited
        // allowlist is cleared, not preserved (a stale list + strict mode
        // would lock the key out of everything).
        conn.execute(
            "UPDATE api_keys SET allowed_models_json = NULL WHERE id = ?1",
            params![key_id.0],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    }
    conn.execute(
        "UPDATE api_keys SET allowed_providers_json = ?1 WHERE id = ?2",
        params![
            config
                .allowed_providers
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|e| openproxy_types::CoreError::Internal(e.to_string()))?,
            key_id.0
        ],
    )
    .map_err(openproxy_db::error::map_db_error)?;
    if let Some(payload) = combos_json {
        conn.execute(
            "UPDATE api_keys SET allowed_combos_json = ?1 WHERE id = ?2",
            params![payload, key_id.0],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    } else {
        conn.execute(
            "UPDATE api_keys SET allowed_combos_json = NULL WHERE id = ?1",
            params![key_id.0],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    }
    if let Some(only) = config.only_whitelisted {
        conn.execute(
            "UPDATE api_keys SET only_whitelisted = ?1 WHERE id = ?2",
            params![i64::from(only), key_id.0],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    }
    Ok(())
}
