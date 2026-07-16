//! Persistent runtime config KV store.

use openproxy_types::{CompressionMode, CoreError, QuotaProtectionConfig, Result, TimeoutsConfig};
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

/// Default value for `idle_chunk_retryable` (false = current behavior).
pub const IDLE_CHUNK_RETRYABLE_DEFAULT: bool = openproxy_types::IDLE_CHUNK_RETRYABLE_DEFAULT;

/// Read the persisted `compression` override, if any.
pub fn load_compression_override_from_db(conn: &Connection) -> Result<Option<CompressionMode>> {
    let mut stmt = conn
        .prepare("SELECT value FROM app_config WHERE key = ?1")
        .map_err(crate::error::map_db_error)?;
    let mut rows = stmt
        .query(params![COMPRESSION_KEY])
        .map_err(crate::error::map_db_error)?;
    match rows.next() {
        Ok(Some(row)) => {
            let raw: String = row.get(0).map_err(crate::error::map_db_error)?;
            match serde_json::from_str::<CompressionMode>(&raw) {
                Ok(cfg) => Ok(Some(cfg)),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        key = COMPRESSION_KEY,
                        "app_config row exists but JSON is corrupt; \
                         ignoring and falling back to config default"
                    );
                    Ok(None)
                }
            }
        }
        Ok(None) => Ok(None),
        Err(e) => Err(crate::error::map_db_error_ctx(
            "iterate load_compression_override",
        )(e)),
    }
}

/// UPSERT the `compression` row.
pub fn save_compression_to_db(
    conn: &Connection,
    mode: &CompressionMode,
    now_unix_secs: i64,
) -> Result<()> {
    let json = serde_json::to_string(mode)
        .map_err(|e| CoreError::Parse(format!("serialize compression mode: {}", e)))?;
    conn.execute(
        "INSERT INTO app_config (key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value,
                                         updated_at = excluded.updated_at",
        params![COMPRESSION_KEY, json, now_unix_secs],
    )
    .map_err(crate::error::map_db_error)?;
    Ok(())
}

/// Read the persisted `idle_chunk_retryable` flag, if any.
pub fn load_idle_chunk_retryable_from_db(conn: &Connection) -> Result<Option<bool>> {
    let mut stmt = conn
        .prepare("SELECT value FROM app_config WHERE key = ?1")
        .map_err(crate::error::map_db_error)?;
    let mut rows = stmt
        .query(params![IDLE_CHUNK_RETRYABLE_KEY])
        .map_err(crate::error::map_db_error)?;
    match rows.next() {
        Ok(Some(row)) => {
            let raw: String = row.get(0).map_err(crate::error::map_db_error)?;
            match serde_json::from_str::<bool>(&raw) {
                Ok(val) => Ok(Some(val)),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        key = IDLE_CHUNK_RETRYABLE_KEY,
                        "app_config row exists but JSON is corrupt; \
                         ignoring and falling back to default"
                    );
                    Ok(None)
                }
            }
        }
        Ok(None) => Ok(None),
        Err(e) => Err(crate::error::map_db_error_ctx(
            "iterate load_idle_chunk_retryable",
        )(e)),
    }
}

/// UPSERT the `idle_chunk_retryable` row.
pub fn save_idle_chunk_retryable_to_db(
    conn: &Connection,
    val: bool,
    now_unix_secs: i64,
) -> Result<()> {
    let json = serde_json::to_string(&val)
        .map_err(|e| CoreError::Parse(format!("serialize idle_chunk_retryable: {}", e)))?;
    conn.execute(
        "INSERT INTO app_config (key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value,
                                         updated_at = excluded.updated_at",
        params![IDLE_CHUNK_RETRYABLE_KEY, json, now_unix_secs],
    )
    .map_err(crate::error::map_db_error)?;
    Ok(())
}

/// Read the persisted `timeouts` override, if any.
pub fn load_timeouts_override_from_db(conn: &Connection) -> Result<Option<TimeoutsConfig>> {
    let mut stmt = conn
        .prepare("SELECT value FROM app_config WHERE key = ?1")
        .map_err(crate::error::map_db_error)?;
    let mut rows = stmt
        .query(params![TIMEOUTS_KEY])
        .map_err(crate::error::map_db_error)?;
    match rows.next() {
        Ok(Some(row)) => {
            let raw: String = row.get(0).map_err(crate::error::map_db_error)?;
            match serde_json::from_str::<TimeoutsConfig>(&raw) {
                Ok(cfg) => Ok(Some(cfg)),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        key = TIMEOUTS_KEY,
                        "app_config row exists but JSON is corrupt; \
                         ignoring and falling back to AppConfig defaults"
                    );
                    Ok(None)
                }
            }
        }
        Ok(None) => Ok(None),
        Err(e) => Err(crate::error::map_db_error_ctx(
            "iterate load_timeouts_override",
        )(e)),
    }
}

/// UPSERT the `timeouts` row.
pub fn save_timeouts_to_db(
    conn: &Connection,
    cfg: &TimeoutsConfig,
    now_unix_secs: i64,
) -> Result<()> {
    let json = serde_json::to_string(cfg)
        .map_err(|e| CoreError::Parse(format!("serialize timeouts: {}", e)))?;
    conn.execute(
        "INSERT INTO app_config (key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value,
                                         updated_at = excluded.updated_at",
        params![TIMEOUTS_KEY, json, now_unix_secs],
    )
    .map_err(crate::error::map_db_error)?;
    Ok(())
}

/// Read the persisted recording TTL, if any.
pub fn load_recording_ttl_from_db(conn: &Connection) -> Result<Option<i64>> {
    let mut stmt = conn
        .prepare("SELECT value FROM app_config WHERE key = ?1")
        .map_err(crate::error::map_db_error)?;
    let mut rows = stmt
        .query(params![RECORDING_TTL_KEY])
        .map_err(crate::error::map_db_error)?;
    match rows.next() {
        Ok(Some(row)) => {
            let raw: String = row.get(0).map_err(crate::error::map_db_error)?;
            match serde_json::from_str::<i64>(&raw) {
                Ok(ttl) => Ok(Some(ttl)),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        key = RECORDING_TTL_KEY,
                        "app_config row exists but JSON is corrupt; \
                         ignoring and falling back to default recording TTL"
                    );
                    Ok(None)
                }
            }
        }
        Ok(None) => Ok(None),
        Err(e) => Err(crate::error::map_db_error_ctx("iterate load_recording_ttl")(e)),
    }
}

/// UPSERT the `recording_ttl_secs` row.
pub fn save_recording_ttl_to_db(
    conn: &Connection,
    ttl_secs: i64,
    now_unix_secs: i64,
) -> Result<()> {
    let json = serde_json::to_string(&ttl_secs)
        .map_err(|e| CoreError::Parse(format!("serialize recording_ttl_secs: {}", e)))?;
    conn.execute(
        "INSERT INTO app_config (key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value,
                                         updated_at = excluded.updated_at",
        params![RECORDING_TTL_KEY, json, now_unix_secs],
    )
    .map_err(crate::error::map_db_error)?;
    Ok(())
}

/// Read the persisted quota protection configuration, if any.
pub fn load_quota_protection_override_from_db(
    conn: &Connection,
) -> Result<Option<QuotaProtectionConfig>> {
    let mut stmt = conn
        .prepare("SELECT value FROM app_config WHERE key = ?1")
        .map_err(crate::error::map_db_error)?;
    let mut rows = stmt
        .query(params![QUOTA_PROTECTION_KEY])
        .map_err(crate::error::map_db_error)?;
    match rows.next() {
        Ok(Some(row)) => {
            let raw: String = row.get(0).map_err(crate::error::map_db_error)?;
            match serde_json::from_str::<QuotaProtectionConfig>(&raw) {
                Ok(cfg) => Ok(Some(cfg)),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        key = QUOTA_PROTECTION_KEY,
                        "app_config row exists but JSON is corrupt; ignoring and falling back to default quota protection"
                    );
                    Ok(None)
                }
            }
        }
        Ok(None) => Ok(None),
        Err(e) => Err(crate::error::map_db_error_ctx(
            "iterate load_quota_protection",
        )(e)),
    }
}

/// UPSERT the `quota_protection` config.
pub fn save_quota_protection_to_db(
    conn: &Connection,
    cfg: &QuotaProtectionConfig,
    now_unix_secs: i64,
) -> Result<()> {
    let json = serde_json::to_string(cfg)
        .map_err(|e| CoreError::Parse(format!("serialize quota_protection: {}", e)))?;
    conn.execute(
        "INSERT INTO app_config (key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value,
                                         updated_at = excluded.updated_at",
        params![QUOTA_PROTECTION_KEY, json, now_unix_secs],
    )
    .map_err(crate::error::map_db_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::DbPool;
    use std::path::PathBuf;

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir();
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = base.join(format!("openproxy-appcfg-test-{}-{}", pid, nanos));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn timeouts_roundtrip_through_db() {
        let dir = tempdir();
        let pool = DbPool::open(&dir.join("rt.db")).unwrap();
        {
            let mut w = pool.writer();
            crate::migrations::run(&mut w).unwrap();
        }
        let original = TimeoutsConfig {
            connect_ms: 1234,
            request_send_ms: 5678,
            ttft_ms: 91011,
            idle_chunk_ms: 121314,
            total_ms: 600000,
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
        let dir = tempdir();
        let pool = DbPool::open(&dir.join("recording-ttl.db")).unwrap();
        {
            let mut w = pool.writer();
            crate::migrations::run(&mut w).unwrap();
        }
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
}
