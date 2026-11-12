//! Credit packages: admin-defined purchase presets.
//!
//! A package is a copy-paste payload: price, balance lifetime and a JSON
//! config that is applied verbatim to the buyer's key when the payment
//! clears (quota caps + model/provider/combo allowlists). The admin defines
//! them once; every purchase inherits the exact same shape.

use openproxy_types::ids::ApiKeyId;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// Key-shaping payload applied when a package payment clears.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PackageConfig {
    pub monthly_usd_cap: Option<f64>,
    pub weekly_usd_cap: Option<f64>,
    pub five_hour_usd_cap: Option<f64>,
    pub allowed_models: Option<Vec<String>>,
    pub allowed_providers: Option<Vec<String>>,
    pub allowed_combos: Option<Vec<String>>,
    pub only_whitelisted: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreditPackage {
    pub id: i64,
    pub name: String,
    pub price_usd: f64,
    pub ttl_days: i64,
    pub config: Option<PackageConfig>,
    pub active: bool,
    /// Balance actually credited when paid. `None` credits `price_usd`
    /// (the legacy 1:1 funnel); `Some(30.0)` on a $5 package is 6x leverage.
    pub credit_usd: Option<f64>,
    /// Tier whose key-shaping applies on payment (wins over `config` gaps).
    pub tier_id: Option<i64>,
}

fn row_to_pkg(row: &rusqlite::Row<'_>) -> rusqlite::Result<CreditPackage> {
    let config_json: Option<String> = row.get(4)?;
    Ok(CreditPackage {
        id: row.get(0)?,
        name: row.get(1)?,
        price_usd: row.get(2)?,
        ttl_days: row.get(3)?,
        config: config_json.and_then(|j| serde_json::from_str(&j).ok()),
        active: row.get::<_, i64>(5)? != 0,
        credit_usd: row.get(6)?,
        tier_id: row.get(7)?,
    })
}

/// List every package (active and inactive; the admin manages both).
///
/// # Errors
/// Propagates DB failures.
pub fn list(conn: &Connection) -> openproxy_types::Result<Vec<CreditPackage>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, price_usd, ttl_days, config_json, active, credit_usd, tier_id \
             FROM credit_packages ORDER BY active DESC, price_usd ASC",
        )
        .map_err(openproxy_db::error::map_db_error)?;
    let rows = stmt
        .query_map([], row_to_pkg)
        .map_err(openproxy_db::error::map_db_error)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(openproxy_db::error::map_db_error)
}

/// Packages a buyer can currently pick (active only).
///
/// # Errors
/// Propagates DB failures.
pub fn list_active(conn: &Connection) -> openproxy_types::Result<Vec<CreditPackage>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, price_usd, ttl_days, config_json, active, credit_usd, tier_id \
             FROM credit_packages WHERE active = 1 ORDER BY price_usd ASC",
        )
        .map_err(openproxy_db::error::map_db_error)?;
    let rows = stmt
        .query_map([], row_to_pkg)
        .map_err(openproxy_db::error::map_db_error)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(openproxy_db::error::map_db_error)
}

/// Create a package.
///
/// # Errors
/// Validation (positive price/ttl) and DB failures (duplicate name).
pub fn create(
    conn: &Connection,
    name: &str,
    price_usd: f64,
    ttl_days: i64,
    config: Option<&PackageConfig>,
    credit_usd: Option<f64>,
    tier_id: Option<i64>,
) -> openproxy_types::Result<CreditPackage> {
    if price_usd <= 0.0 || !price_usd.is_finite() {
        return Err(openproxy_types::CoreError::Validation(
            "price_usd must be positive".into(),
        ));
    }
    if let Some(c) = credit_usd
        && (c <= 0.0 || !c.is_finite())
    {
        return Err(openproxy_types::CoreError::Validation(
            "credit_usd must be positive".into(),
        ));
    }
    if ttl_days <= 0 {
        return Err(openproxy_types::CoreError::Validation(
            "ttl_days must be positive".into(),
        ));
    }
    if let Some(tid) = tier_id
        && crate::credits::tiers::get(conn, tid)?.is_none()
    {
        return Err(openproxy_types::CoreError::Validation(format!(
            "tier {tid} does not exist"
        )));
    }
    let config_json = config.map(|c| serde_json::to_string(c).unwrap_or_default());
    conn.execute(
        "INSERT INTO credit_packages (name, price_usd, ttl_days, config_json, credit_usd, tier_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![name, price_usd, ttl_days, config_json, credit_usd, tier_id],
    )
    .map_err(openproxy_db::error::map_db_error)?;
    let id = conn.last_insert_rowid();
    Ok(CreditPackage {
        id,
        name: name.to_string(),
        price_usd,
        ttl_days,
        config: config.cloned(),
        active: true,
        credit_usd,
        tier_id,
    })
}

/// Update price / ttl / config / active flag. `None` leaves a field as-is.
///
/// # Errors
/// DB failures.
pub fn update(
    conn: &Connection,
    id: i64,
    price_usd: Option<f64>,
    ttl_days: Option<i64>,
    config: Option<&PackageConfig>,
    active: Option<bool>,
    credit_usd: Option<Option<f64>>,
    tier_id: Option<Option<i64>>,
) -> openproxy_types::Result<()> {
    if let Some(p) = price_usd {
        conn.execute(
            "UPDATE credit_packages SET price_usd = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![p, id],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    }
    if let Some(t) = ttl_days {
        conn.execute(
            "UPDATE credit_packages SET ttl_days = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![t, id],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    }
    if let Some(c) = config {
        let config_json = serde_json::to_string(c).unwrap_or_default();
        conn.execute(
            "UPDATE credit_packages SET config_json = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![config_json, id],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    }
    // Double-Option: outer = "touch this field", inner = the value (an inner
    // None clears it, e.g. dropping the leverage back to 1:1).
    if let Some(cu) = credit_usd {
        if let Some(v) = cu
            && (v <= 0.0 || !v.is_finite())
        {
            return Err(openproxy_types::CoreError::Validation(
                "credit_usd must be positive".into(),
            ));
        }
        conn.execute(
            "UPDATE credit_packages SET credit_usd = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![cu, id],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    }
    if let Some(tid) = tier_id {
        if let Some(t) = tid
            && crate::credits::tiers::get(conn, t)?.is_none()
        {
            return Err(openproxy_types::CoreError::Validation(format!(
                "tier {t} does not exist"
            )));
        }
        conn.execute(
            "UPDATE credit_packages SET tier_id = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![tid, id],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    }
    if let Some(a) = active {
        conn.execute(
            "UPDATE credit_packages SET active = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![i64::from(a), id],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    }
    Ok(())
}

/// Fetch one package by id.
///
/// # Errors
/// Propagates DB failures.
pub fn get(conn: &Connection, id: i64) -> openproxy_types::Result<Option<CreditPackage>> {
    conn.query_row(
        "SELECT id, name, price_usd, ttl_days, config_json, active, credit_usd, tier_id \
         FROM credit_packages WHERE id = ?1",
        params![id],
        row_to_pkg,
    )
    .optional()
    .map_err(openproxy_db::error::map_db_error)
}

/// Apply a package's config to a key (copy-paste). Quota caps upsert the
/// existing row; allowlists overwrite; NULL fields leave the key untouched.
///
/// # Errors
/// Propagates DB failures.
pub fn apply_to_key(
    conn: &Connection,
    key_id: ApiKeyId,
    config: &PackageConfig,
) -> openproxy_types::Result<()> {
    let has_any_quota = config.monthly_usd_cap.is_some()
        || config.weekly_usd_cap.is_some()
        || config.five_hour_usd_cap.is_some();
    if has_any_quota {
        let existing =
            crate::api_key_quotas::get_caps(conn, key_id)?
                .unwrap_or(crate::api_key_quotas::QuotaCaps {
                    monthly_usd_cap: None,
                    weekly_usd_cap: None,
                    five_hour_usd_cap: None,
                });
        let caps = crate::api_key_quotas::QuotaCaps {
            monthly_usd_cap: config.monthly_usd_cap.or(existing.monthly_usd_cap),
            weekly_usd_cap: config.weekly_usd_cap.or(existing.weekly_usd_cap),
            five_hour_usd_cap: config.five_hour_usd_cap.or(existing.five_hour_usd_cap),
        };
        crate::api_key_quotas::upsert(conn, key_id, &caps)?;
    }
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
    }
    if let Some(providers) = &config.allowed_providers {
        conn.execute(
            "UPDATE api_keys SET allowed_providers_json = ?1 WHERE id = ?2",
            params![serde_json::to_string(providers).unwrap_or_default(), key_id.0],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    }
    if let Some(payload) = combos_json {
        conn.execute(
            "UPDATE api_keys SET allowed_combos_json = ?1 WHERE id = ?2",
            params![payload, key_id.0],
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
    let models_json = models
        .map(|_| serde_json::to_string(&model_ids).unwrap_or_else(|_| "[]".into()));
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
pub(crate) fn partition_names(
    conn: &Connection,
    models: Option<&Vec<String>>,
    combos: Option<&Vec<String>>,
) -> openproxy_types::Result<(Option<Vec<String>>, Option<Vec<String>>)> {
    match (models, combos) {
        (None, None) => return Ok((None, None)),
        _ => {}
    }
    let mut model_ids: Vec<String> = Vec::new();
    let mut combo_names: Vec<String> = Vec::new();
    let mut is_combo_name = |conn: &Connection, entry: &str| -> openproxy_types::Result<bool> {
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
    let combos_out = if combo_names.is_empty() {
        None
    } else {
        Some(combo_names)
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
pub fn tier_own_config(tier: &crate::credits::tiers::CreditTier) -> PackageConfig {
    let t = &tier.config;
    // Propagation always writes a definite strict-mode flag: an unrestricted
    // tier must CLEAR only_whitelisted (a stale 1 with cleared allowlists
    // denies every request).
    let gate = Some(
        t.only_whitelisted
            .unwrap_or_else(|| t.allowed_models.is_some() || t.allowed_providers.is_some()),
    );
    PackageConfig {
        monthly_usd_cap: t.monthly_usd_cap,
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
pub fn propagate_tier_to_keys(
    conn: &Connection,
    tier_id: i64,
) -> openproxy_types::Result<usize> {
    let Some(tier) = crate::credits::tiers::get(conn, tier_id)? else {
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

/// Like `apply_to_key` but authoritative: quota caps are SET (a `None` clears
/// the previous cap) and allowlists are written even when the tier no longer
/// constrains them. Used by tier propagation, where the tier is the truth.
///
/// # Errors
/// Propagates DB failures.
pub fn apply_to_key_forced(
    conn: &Connection,
    key_id: ApiKeyId,
    config: &PackageConfig,
) -> openproxy_types::Result<()> {
    let caps = crate::api_key_quotas::QuotaCaps {
        monthly_usd_cap: config.monthly_usd_cap,
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
    if let Some(providers) = &config.allowed_providers {
        conn.execute(
            "UPDATE api_keys SET allowed_providers_json = ?1 WHERE id = ?2",
            params![serde_json::to_string(providers).unwrap_or_default(), key_id.0],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    }
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

/// Effective key-shaping of a purchase: the tier is the access contract, the
/// package's own legacy `config_json` only fills fields the tier left unset
/// (`None`). A package without tier keeps the pure legacy behaviour.
///
/// # Errors
/// Propagates DB failures.
pub fn resolved_config(
    conn: &Connection,
    pkg: &CreditPackage,
) -> openproxy_types::Result<Option<PackageConfig>> {
    let Some(tid) = pkg.tier_id else {
        return Ok(pkg.config.clone());
    };
    let Some(tier) = crate::credits::tiers::get(conn, tid)? else {
        // Tier deleted out from under the package: fall back to the package's
        // own config so a paid order still shapes the key deterministically.
        return Ok(pkg.config.clone());
    };
    let t = &tier.config;
    let pc = pkg.config.as_ref();
    // Money invariant, at the resolution point: when the tier carries an
    // allowlist the merged config carries the hard gate, whichever apply
    // path (tiers:: or packages::apply_to_key) consumes it.
    let tier_gate = t
        .only_whitelisted
        .or(if t.allowed_models.is_some() { Some(true) } else { None });
    let merged = PackageConfig {
        monthly_usd_cap: t.monthly_usd_cap.or(pc.and_then(|c| c.monthly_usd_cap)),
        weekly_usd_cap: t.weekly_usd_cap.or(pc.and_then(|c| c.weekly_usd_cap)),
        five_hour_usd_cap: t
            .five_hour_usd_cap
            .or(pc.and_then(|c| c.five_hour_usd_cap)),
        allowed_models: t
            .allowed_models
            .clone()
            .or_else(|| pc.and_then(|c| c.allowed_models.clone())),
        allowed_providers: t
            .allowed_providers
            .clone()
            .or_else(|| pc.and_then(|c| c.allowed_providers.clone())),
        allowed_combos: t
            .allowed_combos
            .clone()
            .or_else(|| pc.and_then(|c| c.allowed_combos.clone())),
        only_whitelisted: tier_gate.or(pc.and_then(|c| c.only_whitelisted)),
    };
    Ok(Some(merged))
}

/// Micro-USDC credited when a package payment clears: the leverage point.
/// `None` credit_usd keeps the legacy credit == price behaviour.
#[must_use]
pub fn credit_units_of(pkg: &CreditPackage) -> i64 {
    let usd = pkg.credit_usd.unwrap_or(pkg.price_usd);
    (usd * f64::from(crate::credits::UNITS_PER_USD as u32)).round() as i64
}
