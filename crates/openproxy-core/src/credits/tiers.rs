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
    pub monthly_usd_cap: Option<f64>,
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
    /// How many packages point at this tier (a tier in use cannot be deleted).
    pub package_count: i64,
    /// How many live keys inherited this tier (the single-source-of-truth
    /// propagation surface: an edit re-shapes exactly this many keys).
    pub key_count: i64,
}

const TIER_COLS: &str = "t.id, t.name, t.models_json, t.providers_json, t.combos_json, \
                         t.only_whitelisted, t.monthly_usd_cap, t.weekly_usd_cap, \
                         t.five_hour_usd_cap, t.active";

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
            monthly_usd_cap: row.get(6)?,
            weekly_usd_cap: row.get(7)?,
            five_hour_usd_cap: row.get(8)?,
        },
        active: row.get::<_, i64>(9)? != 0,
        package_count: row.get(10).unwrap_or(0),
        key_count: row.get(11).unwrap_or(0),
    })
}

/// Every tier, with the number of packages referencing it.
///
/// # Errors
/// Propagates DB failures.
pub fn list(conn: &Connection) -> openproxy_types::Result<Vec<CreditTier>> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {TIER_COLS}, \
             (SELECT COUNT(*) FROM credit_packages p WHERE p.tier_id = t.id), \
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
             (SELECT COUNT(*) FROM credit_packages p WHERE p.tier_id = t.id), \
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

fn sanitize(config: &TierConfig) -> TierConfig {
    fn cap(v: Option<f64>) -> Option<f64> {
        match v {
            Some(x) if x.is_finite() && x >= 0.0 => Some(x),
            _ => None,
        }
    }
    TierConfig {
        monthly_usd_cap: cap(config.monthly_usd_cap),
        weekly_usd_cap: cap(config.weekly_usd_cap),
        five_hour_usd_cap: cap(config.five_hour_usd_cap),
        allowed_models: config.allowed_models.clone(),
        allowed_providers: config.allowed_providers.clone(),
        allowed_combos: config.allowed_combos.clone(),
        only_whitelisted: config.only_whitelisted,
    }
}

/// Create a tier. Duplicate names are rejected by the UNIQUE index.
///
/// # Errors
/// Validation and DB failures.
pub fn create(
    conn: &Connection,
    name: &str,
    config: &TierConfig,
) -> openproxy_types::Result<CreditTier> {
    let name = validate_name(name)?;
    let cfg = sanitize(config);
    // The admin UI sends ONE merged selection; combo entries (`combo:` prefix
    // or names matching a live combo row) must land in combos_json or the
    // routing gate never matches them. Store a canonical row.
    let (models, combos) = crate::credits::packages::partition_names(
        conn,
        cfg.allowed_models.as_ref(),
        cfg.allowed_combos.as_ref(),
    )?;
    conn.execute(
        "INSERT INTO credit_tiers (name, models_json, providers_json, combos_json, \
             only_whitelisted, monthly_usd_cap, weekly_usd_cap, five_hour_usd_cap) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
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
            cfg.monthly_usd_cap,
            cfg.weekly_usd_cap,
            cfg.five_hour_usd_cap,
        ],
    )
    .map_err(openproxy_db::error::map_db_error)?;
    let id = conn.last_insert_rowid();
    get(conn, id)?.ok_or_else(|| {
        openproxy_types::CoreError::Internal("tier vanished after insert".into())
    })
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
        let (models, combos) = crate::credits::packages::partition_names(
            conn,
            cfg.allowed_models.as_ref(),
            cfg.allowed_combos.as_ref(),
        )?;
        conn.execute(
            "UPDATE credit_tiers SET models_json = ?1, providers_json = ?2, combos_json = ?3, \
                 only_whitelisted = ?4, monthly_usd_cap = ?5, weekly_usd_cap = ?6, \
                 five_hour_usd_cap = ?7, updated_at = datetime('now') WHERE id = ?8",
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
                cfg.monthly_usd_cap,
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
    // Single source of truth: a config edit re-shapes every key linked to
    // this tier, in the same transaction (Miguel, 2026-11-12). Active
    // toggles only affect NEW purchases, not already-linked keys.
    if config.is_some() {
        let keys = crate::credits::packages::propagate_tier_to_keys(conn, id)?;
        if keys > 0 {
            tracing::info!(tier_id = id, keys, "tier propagated to linked keys");
        }
    }
    Ok(())
}

/// Delete a tier that no package references. Tiers in use are deactivated
/// instead by the caller (deleting one would orphan its packages).
///
/// # Errors
/// - [`openproxy_types::CoreError::Validation`] when packages still use it.
/// - Propagates DB failures.
pub fn delete(conn: &Connection, id: i64) -> openproxy_types::Result<()> {
    if crate::credits::tiers::get(conn, id)?.is_none() {
        return Ok(());
    }
    let used: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM credit_packages WHERE tier_id = ?1",
            params![id],
            |r| r.get(0),
        )
        .map_err(openproxy_db::error::map_db_error)?;
    if used > 0 {
        return Err(openproxy_types::CoreError::Validation(format!(
            "tier is used by {used} package(s): deactivate it instead"
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
        monthly_usd_cap: config.monthly_usd_cap,
        weekly_usd_cap: config.weekly_usd_cap,
        five_hour_usd_cap: config.five_hour_usd_cap,
    };
    crate::api_key_quotas::upsert(conn, key_id, &caps)?;

    // Combo entries may ride in allowed_models (merged admin selection);
    // partition before writing, and store combo ids — the shape the routing
    // gate consumes (`is_combo_allowed`).
    let (models, combos) = crate::credits::packages::partition_names(
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
        let ids = crate::credits::packages::resolve_combo_ids(conn, combos)?;
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
