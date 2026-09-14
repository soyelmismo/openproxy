//! Persistent runtime config KV store.

use openproxy_types::{
    CompressionMode, CoreError, PiiConfig, PiiEntity, QuotaProtectionConfig, Result, TimeoutsConfig,
};
use rusqlite::{Connection, params};

/// Key under which the [`TimeoutsConfig`] override is stored.
pub const TIMEOUTS_KEY: &str = "timeouts";

/// Key under which the recording TTL (seconds) is stored.
pub const RECORDING_TTL_KEY: &str = "recording_ttl_secs";

/// Default recording body TTL in seconds (5 minutes).
pub const RECORDING_TTL_DEFAULT_SECS: i64 = 300;

/// Key under which the compression mode override is stored.
pub const COMPRESSION_KEY: &str = "compression";

/// Key under which the `quota_protection` config is stored.
pub const QUOTA_PROTECTION_KEY: &str = "quota_protection";

/// Key under which the `idle_chunk_retryable` flag is stored.
pub const IDLE_CHUNK_RETRYABLE_KEY: &str = "idle_chunk_retryable";

/// Key under which the PII config is stored.
pub const PII_CONFIG_KEY: &str = "pii_config";
pub const PII_ENABLED_KEY: &str = "pii_enabled";
pub const PII_REVERSIBLE_KEY: &str = "pii_reversible";
pub const PII_REDACT_LOGS_KEY: &str = "pii_redact_logs";
pub const PII_ENTITIES_KEY: &str = "pii_entities";

/// Default value for `idle_chunk_retryable` (false = current behavior).
pub const IDLE_CHUNK_RETRYABLE_DEFAULT: bool = openproxy_types::IDLE_CHUNK_RETRYABLE_DEFAULT;

pub const PROXY_TEST_URL_KEY: &str = "proxy_test_url";
pub const PROXY_TEST_URL_DEFAULT: &str = "https://cloudflare.com/cdn-cgi/trace";

use rusqlite::OptionalExtension;

fn parse_config_json<T: serde::de::DeserializeOwned>(raw: &str, key: &str) -> Option<T> {
    match serde_json::from_str::<T>(raw) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            tracing::warn!(
                error = %e,
                key = key,
                "app_config row exists but JSON is corrupt; ignoring and falling back to default"
            );
            None
        }
    }
}

fn load_config_val<T: serde::de::DeserializeOwned>(
    conn: &Connection,
    key: &str,
) -> Result<Option<T>> {
    let raw_opt: Option<String> = conn
        .query_row(
            "SELECT value FROM app_config WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .map_err(crate::error::map_db_error_ctx("query load_config_val"))?;

    Ok(raw_opt.and_then(|raw| parse_config_json(&raw, key)))
}

fn save_config_val<T: serde::Serialize>(
    conn: &Connection,
    key: &str,
    val: &T,
    now_unix_secs: i64,
) -> Result<()> {
    let json = serde_json::to_string(val)
        .map_err(|e| CoreError::Parse(format!("serialize {key}: {e}")))?;
    conn.execute(
        "INSERT INTO app_config (key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value,
                                         updated_at = excluded.updated_at",
        params![key, json, now_unix_secs],
    )
    .map_err(crate::error::map_db_error)?;
    Ok(())
}

macro_rules! app_config_kv {
    (
        $(
            $(#[$meta:meta])*
            $load_fn:ident, $save_fn:ident, $key:expr, &$ty:ty;
        )*
    ) => {
        $(
            $(#[$meta])*
            pub fn $load_fn(conn: &Connection) -> Result<Option<$ty>> {
                load_config_val(conn, $key)
            }

            $(#[$meta])*
            pub fn $save_fn(
                conn: &Connection,
                val: &$ty,
                now_unix_secs: i64,
            ) -> Result<()> {
                save_config_val(conn, $key, val, now_unix_secs)
            }
        )*
    };
    (
        $(
            $(#[$meta:meta])*
            $load_fn:ident, $save_fn:ident, $key:expr, $ty:ty;
        )*
    ) => {
        $(
            $(#[$meta])*
            pub fn $load_fn(conn: &Connection) -> Result<Option<$ty>> {
                load_config_val(conn, $key)
            }

            $(#[$meta])*
            pub fn $save_fn(
                conn: &Connection,
                val: $ty,
                now_unix_secs: i64,
            ) -> Result<()> {
                save_config_val(conn, $key, &val, now_unix_secs)
            }
        )*
    };
}

app_config_kv! {
    /// Compression mode override.
    load_compression_override_from_db, save_compression_to_db, COMPRESSION_KEY, &CompressionMode;
    /// Timeouts override.
    load_timeouts_override_from_db, save_timeouts_to_db, TIMEOUTS_KEY, &TimeoutsConfig;
    /// Quota protection override.
    load_quota_protection_override_from_db, save_quota_protection_to_db, QUOTA_PROTECTION_KEY, &QuotaProtectionConfig;
}

app_config_kv! {
    /// `idle_chunk_retryable` flag.
    load_idle_chunk_retryable_from_db, save_idle_chunk_retryable_to_db, IDLE_CHUNK_RETRYABLE_KEY, bool;
    /// Recording TTL in seconds.
    load_recording_ttl_from_db, save_recording_ttl_to_db, RECORDING_TTL_KEY, i64;
}

pub fn load_pii_config_from_db(conn: &Connection) -> Result<Option<PiiConfig>> {
    if let Some(cfg) = load_config_val::<PiiConfig>(conn, PII_CONFIG_KEY)? {
        return Ok(Some(cfg));
    }

    let enabled = load_config_val::<bool>(conn, PII_ENABLED_KEY)?;
    let reversible = load_config_val::<bool>(conn, PII_REVERSIBLE_KEY)?;
    let redact_logs = load_config_val::<bool>(conn, PII_REDACT_LOGS_KEY)?;
    let entities = load_config_val::<Vec<PiiEntity>>(conn, PII_ENTITIES_KEY)?;

    if enabled.is_none() && reversible.is_none() && redact_logs.is_none() && entities.is_none() {
        return Ok(None);
    }

    let def = PiiConfig::default();
    Ok(Some(PiiConfig {
        pii_enabled: enabled.unwrap_or(def.pii_enabled),
        pii_reversible: reversible.unwrap_or(def.pii_reversible),
        pii_redact_logs: redact_logs.unwrap_or(def.pii_redact_logs),
        pii_entities: entities.unwrap_or(def.pii_entities),
    }))
}

pub fn save_pii_config_to_db(conn: &Connection, cfg: &PiiConfig, now_unix_secs: i64) -> Result<()> {
    let mut clean_cfg = cfg.clone();
    let mut seen = std::collections::HashSet::with_capacity(clean_cfg.pii_entities.len());
    clean_cfg.pii_entities.retain(|e| seen.insert(*e));
    save_config_val(conn, PII_CONFIG_KEY, &clean_cfg, now_unix_secs)?;
    save_config_val(conn, PII_ENABLED_KEY, &clean_cfg.pii_enabled, now_unix_secs)?;
    save_config_val(
        conn,
        PII_REVERSIBLE_KEY,
        &clean_cfg.pii_reversible,
        now_unix_secs,
    )?;
    save_config_val(
        conn,
        PII_REDACT_LOGS_KEY,
        &clean_cfg.pii_redact_logs,
        now_unix_secs,
    )?;
    save_config_val(
        conn,
        PII_ENTITIES_KEY,
        &clean_cfg.pii_entities,
        now_unix_secs,
    )?;
    Ok(())
}

pub fn load_proxy_test_url(conn: &Connection) -> Result<String> {
    let raw_opt: Option<String> = conn
        .query_row(
            "SELECT value FROM app_config WHERE key = ?1",
            params![PROXY_TEST_URL_KEY],
            |row| row.get(0),
        )
        .optional()
        .map_err(crate::error::map_db_error)?;

    match raw_opt {
        Some(raw) => Ok(serde_json::from_str::<String>(&raw).unwrap_or(raw)),
        None => Ok(PROXY_TEST_URL_DEFAULT.to_string()),
    }
}

pub fn save_proxy_test_url(conn: &Connection, url: &str) -> Result<()> {
    let raw = serde_json::to_string(url).map_err(crate::error::map_db_error)?;
    let now = chrono::Utc::now().timestamp();
    conn.execute(
        "INSERT INTO app_config (key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = ?2, updated_at = ?3",
        params![PROXY_TEST_URL_KEY, raw, now],
    )
    .map_err(crate::error::map_db_error)?;
    Ok(())
}

/// Default value for notifications_enabled flag.
pub const NOTIFICATIONS_ENABLED_DEFAULT: bool = true;

/// Load notifications_enabled flag. Defaults to true when the row is absent.
pub fn load_notifications_enabled_from_db(conn: &Connection) -> Result<Option<bool>> {
    let raw_opt: Option<String> = conn
        .query_row(
            "SELECT value FROM app_config WHERE key = 'notifications_enabled'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(crate::error::map_db_error)?;

    match raw_opt {
        Some(raw) => Ok(Some(serde_json::from_str::<bool>(&raw).unwrap_or(true))),
        None => Ok(Some(true)),
    }
}

/// Save notifications_enabled flag.
pub fn save_notifications_enabled_to_db(conn: &Connection, value: bool, _now: i64) -> Result<()> {
    let raw = serde_json::to_string(&value).map_err(crate::error::map_db_error)?;
    let now = chrono::Utc::now().timestamp();
    conn.execute(
        "INSERT INTO app_config (key, value, updated_at) VALUES ('notifications_enabled', ?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = ?1, updated_at = ?2",
        params![raw, now],
    )
    .map_err(crate::error::map_db_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::DbPool;

    #[test]
    fn timeouts_roundtrip_through_db() {
        let pool = DbPool::test_pool_with_prefix("openproxy-appcfg-test").unwrap();
        let original = TimeoutsConfig {
            connect_ms: 1234,
            request_send_ms: 5678,
            ttft_ms: 91011,
            idle_chunk_ms: 121_314,
            total_ms: 600_000,
        };
        {
            let w = pool.writer();
            save_timeouts_to_db(&w, &original, 1_700_000_000).unwrap();
        }
        let read_back = {
            let w = pool.writer();
            load_timeouts_override_from_db(&w).unwrap()
        };
        assert_eq!(read_back, Some(original));
    }

    #[test]
    fn recording_ttl_roundtrip_through_db() {
        let pool = DbPool::test_pool_with_prefix("openproxy-appcfg-ttl").unwrap();
        {
            let w = pool.writer();
            save_recording_ttl_to_db(&w, 123, 1_700_000_002).unwrap();
        }
        let got = {
            let w = pool.writer();
            load_recording_ttl_from_db(&w).unwrap()
        };
        assert_eq!(got, Some(123));
    }

    #[test]
    fn pii_config_roundtrip_through_db() {
        let pool = DbPool::test_pool_with_prefix("openproxy-appcfg-pii").unwrap();
        let original = PiiConfig {
            pii_enabled: true,
            pii_reversible: true,
            pii_redact_logs: false,
            pii_entities: vec![PiiEntity::Email, PiiEntity::CreditCard, PiiEntity::Secret],
        };
        {
            let w = pool.writer();
            save_pii_config_to_db(&w, &original, 1_700_000_003).unwrap();
        }
        let got = {
            let w = pool.writer();
            load_pii_config_from_db(&w).unwrap()
        };
        assert_eq!(got, Some(original));

        // Verify that separate rows for pii_enabled, pii_reversible, pii_redact_logs, and pii_entities exist in app_config
        {
            let w = pool.writer();
            let count: i64 = w
                .query_row(
                    "SELECT COUNT(*) FROM app_config WHERE key IN ('pii_config', 'pii_enabled', 'pii_reversible', 'pii_redact_logs', 'pii_entities')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 5);
        }
    }

    #[test]
    fn notifications_enabled_roundtrip_through_db() {
        let pool = DbPool::test_pool_with_prefix("openproxy-appcfg-notif").unwrap();

        // Default is true even when no row exists yet
        {
            let w = pool.writer();
            let got = load_notifications_enabled_from_db(&w).unwrap();
            assert_eq!(got, Some(true));
        }

        // Save false
        {
            let w = pool.writer();
            save_notifications_enabled_to_db(&w, false, 1_700_000_004).unwrap();
        }
        {
            let w = pool.writer();
            let got = load_notifications_enabled_from_db(&w).unwrap();
            assert_eq!(got, Some(false));
        }

        // Save true again
        {
            let w = pool.writer();
            save_notifications_enabled_to_db(&w, true, 1_700_000_005).unwrap();
        }
        {
            let w = pool.writer();
            let got = load_notifications_enabled_from_db(&w).unwrap();
            assert_eq!(got, Some(true));
        }
    }
}
