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
    })
}

/// List every package (active and inactive; the admin manages both).
///
/// # Errors
/// Propagates DB failures.
pub fn list(conn: &Connection) -> openproxy_types::Result<Vec<CreditPackage>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, price_usd, ttl_days, config_json, active \
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
            "SELECT id, name, price_usd, ttl_days, config_json, active \
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
) -> openproxy_types::Result<CreditPackage> {
    if price_usd <= 0.0 || !price_usd.is_finite() {
        return Err(openproxy_types::CoreError::Validation(
            "price_usd must be positive".into(),
        ));
    }
    if ttl_days <= 0 {
        return Err(openproxy_types::CoreError::Validation(
            "ttl_days must be positive".into(),
        ));
    }
    let config_json = config.map(|c| serde_json::to_string(c).unwrap_or_default());
    conn.execute(
        "INSERT INTO credit_packages (name, price_usd, ttl_days, config_json) \
         VALUES (?1, ?2, ?3, ?4)",
        params![name, price_usd, ttl_days, config_json],
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
        "SELECT id, name, price_usd, ttl_days, config_json, active \
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
    if let Some(models) = &config.allowed_models {
        conn.execute(
            "UPDATE api_keys SET allowed_models_json = ?1 WHERE id = ?2",
            params![serde_json::to_string(models).unwrap_or_default(), key_id.0],
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
    if let Some(combos) = &config.allowed_combos {
        conn.execute(
            "UPDATE api_keys SET allowed_combos_json = ?1 WHERE id = ?2",
            params![serde_json::to_string(combos).unwrap_or_default(), key_id.0],
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
