//! Persistent runtime config KV store.
//!
//! The `app_config` table is a tiny key/value sidecar for values that
//! operators may want to edit from the dashboard without restarting
//! the server. The MVP only stores one key (`timeouts` — the
//! [`crate::config::TimeoutsConfig`]); the table is generic so future
//! keys can be added without schema changes.
//!
//! ## Concurrency model
//!
//! - **Writes** go through the [`crate::db::conn::DbPool::writer`]
//!   handle, which is serialized by a `parking_lot::Mutex`. The
//!   `INSERT ... ON CONFLICT(key) DO UPDATE` is atomic in SQLite.
//! - **Reads** are also taken from the writer handle today. A future
//!   reader-side helper could move to the `reader()` handle; for the
//!   small one-row case the writer is fine and keeps the code
//!   single-path.
//!
//! ## Defensive JSON handling
//!
//! If an external script corrupts the JSON in the `value` column,
//! [`load_timeouts_override_from_db`] logs a warning and returns
//! `Ok(None)` so the server keeps booting with the TOML defaults
//! rather than dying. A subsequent `PUT` will overwrite the corrupt
//! row with a valid value.

use crate::compression::CompressionMode;
use crate::config::TimeoutsConfig;
use crate::error::{CoreError, Result};
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
pub const IDLE_CHUNK_RETRYABLE_DEFAULT: bool = crate::config::IDLE_CHUNK_RETRYABLE_DEFAULT;

/// Read the persisted `compression` override, if any.
pub fn load_compression_override_from_db(conn: &Connection) -> Result<Option<CompressionMode>> {
    let mut stmt = conn
        .prepare("SELECT value FROM app_config WHERE key = ?1")
        .map_err(|e| CoreError::Database {
            message: format!("prepare load_compression_override: {}", e),
            source: Some(Box::new(e)),
        })?;
    let mut rows = stmt
        .query(params![COMPRESSION_KEY])
        .map_err(|e| CoreError::Database {
            message: format!("query load_compression_override: {}", e),
            source: Some(Box::new(e)),
        })?;
    match rows.next() {
        Ok(Some(row)) => {
            let raw: String = row.get(0).map_err(|e| CoreError::Database {
                message: format!("read app_config.value: {}", e),
                source: Some(Box::new(e)),
            })?;
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
        Err(e) => Err(CoreError::Database {
            message: format!("iterate load_compression_override: {}", e),
            source: Some(Box::new(e)),
        }),
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
    .map_err(|e| CoreError::Database {
        message: format!("upsert app_config.compression: {}", e),
        source: Some(Box::new(e)),
    })?;
    Ok(())
}

/// Read the persisted `idle_chunk_retryable` flag, if any.
///
/// Returns `Ok(Some(bool))` if a row exists and parses cleanly,
/// `Ok(None)` if no row or corrupt JSON (falls back to default).
pub fn load_idle_chunk_retryable_from_db(conn: &Connection) -> Result<Option<bool>> {
    let mut stmt = conn
        .prepare("SELECT value FROM app_config WHERE key = ?1")
        .map_err(|e| CoreError::Database {
            message: format!("prepare load_idle_chunk_retryable: {}", e),
            source: Some(Box::new(e)),
        })?;
    let mut rows =
        stmt.query(params![IDLE_CHUNK_RETRYABLE_KEY])
            .map_err(|e| CoreError::Database {
                message: format!("query load_idle_chunk_retryable: {}", e),
                source: Some(Box::new(e)),
            })?;
    match rows.next() {
        Ok(Some(row)) => {
            let raw: String = row.get(0).map_err(|e| CoreError::Database {
                message: format!("read app_config.value: {}", e),
                source: Some(Box::new(e)),
            })?;
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
        Err(e) => Err(CoreError::Database {
            message: format!("iterate load_idle_chunk_retryable: {}", e),
            source: Some(Box::new(e)),
        }),
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
    .map_err(|e| CoreError::Database {
        message: format!("upsert app_config.idle_chunk_retryable: {}", e),
        source: Some(Box::new(e)),
    })?;
    Ok(())
}

/// Read the persisted `timeouts` override, if any.
///
/// Returns:
/// - `Ok(Some(cfg))` if a row exists and parses cleanly.
/// - `Ok(None)` if no row exists, **or** the row exists but the JSON
///   is corrupt (logged at `WARN` — see §10 R2 of the spec).
/// - `Err(CoreError::Database { .. })` only for actual DB I/O errors.
pub fn load_timeouts_override_from_db(conn: &Connection) -> Result<Option<TimeoutsConfig>> {
    let mut stmt = conn
        .prepare("SELECT value FROM app_config WHERE key = ?1")
        .map_err(|e| CoreError::Database {
            message: format!("prepare load_timeouts_override: {}", e),
            source: Some(Box::new(e)),
        })?;
    let mut rows = stmt
        .query(params![TIMEOUTS_KEY])
        .map_err(|e| CoreError::Database {
            message: format!("query load_timeouts_override: {}", e),
            source: Some(Box::new(e)),
        })?;
    match rows.next() {
        Ok(Some(row)) => {
            let raw: String = row.get(0).map_err(|e| CoreError::Database {
                message: format!("read app_config.value: {}", e),
                source: Some(Box::new(e)),
            })?;
            match serde_json::from_str::<TimeoutsConfig>(&raw) {
                Ok(cfg) => Ok(Some(cfg)),
                Err(e) => {
                    // Defensive: a corrupt row should NOT prevent the
                    // server from booting. Log loudly and fall back
                    // to the AppConfig defaults; a subsequent PUT
                    // will overwrite the garbage.
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
        Err(e) => Err(CoreError::Database {
            message: format!("iterate load_timeouts_override: {}", e),
            source: Some(Box::new(e)),
        }),
    }
}

/// UPSERT the `timeouts` row.
///
/// `now_unix_secs` is injected by the caller (typically
/// `chrono::Utc::now().timestamp()`) so this module doesn't depend on
/// `chrono` directly. The `INSERT ... ON CONFLICT(key) DO UPDATE` form
/// makes the operation idempotent and atomic in a single SQL
/// statement.
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
    .map_err(|e| CoreError::Database {
        message: format!("upsert app_config.timeouts: {}", e),
        source: Some(Box::new(e)),
    })?;
    Ok(())
}

/// Read the persisted recording TTL, if any.
///
/// Returns:
/// - `Ok(Some(ttl_secs))` if a row exists and parses cleanly.
/// - `Ok(None)` if no row exists, **or** the row exists but the JSON
///   is corrupt (logged at `WARN` — see §10 R2 of the spec).
/// - `Err(CoreError::Database { .. })` only for actual DB I/O errors.
pub fn load_recording_ttl_from_db(conn: &Connection) -> Result<Option<i64>> {
    let mut stmt = conn
        .prepare("SELECT value FROM app_config WHERE key = ?1")
        .map_err(|e| CoreError::Database {
            message: format!("prepare load_recording_ttl: {}", e),
            source: Some(Box::new(e)),
        })?;
    let mut rows = stmt
        .query(params![RECORDING_TTL_KEY])
        .map_err(|e| CoreError::Database {
            message: format!("query load_recording_ttl: {}", e),
            source: Some(Box::new(e)),
        })?;
    match rows.next() {
        Ok(Some(row)) => {
            let raw: String = row.get(0).map_err(|e| CoreError::Database {
                message: format!("read app_config.value: {}", e),
                source: Some(Box::new(e)),
            })?;
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
        Err(e) => Err(CoreError::Database {
            message: format!("iterate load_recording_ttl: {}", e),
            source: Some(Box::new(e)),
        }),
    }
}

/// UPSERT the `recording_ttl_secs` row.
///
/// `ttl_secs` is the TTL in seconds for recorded request/response bodies
/// and headers in the `usage` table. A value of `0` disables body/header
/// retention entirely.
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
    .map_err(|e| CoreError::Database {
        message: format!("upsert app_config.recording_ttl_secs: {}", e),
        source: Some(Box::new(e)),
    })?;
    Ok(())
}

/// Read the persisted quota protection configuration, if any.
pub fn load_quota_protection_override_from_db(
    conn: &Connection,
) -> Result<Option<crate::config::QuotaProtectionConfig>> {
    let mut stmt = conn
        .prepare("SELECT value FROM app_config WHERE key = ?1")
        .map_err(|e| CoreError::Database {
            message: format!("prepare load_quota_protection: {}", e),
            source: Some(Box::new(e)),
        })?;
    let mut rows = stmt
        .query(params![QUOTA_PROTECTION_KEY])
        .map_err(|e| CoreError::Database {
            message: format!("query load_quota_protection: {}", e),
            source: Some(Box::new(e)),
        })?;
    match rows.next() {
        Ok(Some(row)) => {
            let raw: String = row.get(0).map_err(|e| CoreError::Database {
                message: format!("read app_config.value: {}", e),
                source: Some(Box::new(e)),
            })?;
            match serde_json::from_str::<crate::config::QuotaProtectionConfig>(&raw) {
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
        Err(e) => Err(CoreError::Database {
            message: format!("iterate load_quota_protection: {}", e),
            source: Some(Box::new(e)),
        }),
    }
}

/// UPSERT the `quota_protection` config.
pub fn save_quota_protection_to_db(
    conn: &Connection,
    cfg: &crate::config::QuotaProtectionConfig,
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
    .map_err(|e| CoreError::Database {
        message: format!("upsert app_config.quota_protection: {}", e),
        source: Some(Box::new(e)),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::conn::DbPool;
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
            crate::db::migrations::run(&mut w).unwrap();
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
            crate::db::migrations::run(&mut w).unwrap();
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

    #[test]
    fn load_returns_none_when_no_row() {
        let dir = tempdir();
        let pool = DbPool::open(&dir.join("none.db")).unwrap();
        {
            let mut w = pool.writer();
            crate::db::migrations::run(&mut w).unwrap();
        }
        let got = {
            let w = pool.writer();
            load_timeouts_override_from_db(&w).unwrap()
        };
        assert_eq!(got, None);
    }

    #[test]
    fn load_returns_value_when_row_exists() {
        let dir = tempdir();
        let pool = DbPool::open(&dir.join("yes.db")).unwrap();
        {
            let mut w = pool.writer();
            crate::db::migrations::run(&mut w).unwrap();
        }
        let cfg = TimeoutsConfig::default();
        {
            let w = pool.writer();
            save_timeouts_to_db(&w, &cfg, 1_700_000_001).unwrap();
        }
        let got = {
            let w = pool.writer();
            load_timeouts_override_from_db(&w).unwrap()
        };
        assert_eq!(got, Some(cfg));
    }

    #[test]
    fn save_is_idempotent() {
        let dir = tempdir();
        let pool = DbPool::open(&dir.join("idem.db")).unwrap();
        {
            let mut w = pool.writer();
            crate::db::migrations::run(&mut w).unwrap();
        }
        let cfg = TimeoutsConfig::default();
        {
            let w = pool.writer();
            save_timeouts_to_db(&w, &cfg, 1).unwrap();
            save_timeouts_to_db(&w, &cfg, 2).unwrap();
        }
        // Only one row for the key.
        let count: i64 = pool.with_conn(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM app_config WHERE key = ?1",
                params![TIMEOUTS_KEY],
                |r| r.get(0),
            )
            .unwrap()
        });
        assert_eq!(count, 1, "UPSERT must collapse to a single row");
        // updated_at reflects the last write.
        let updated_at: i64 = pool.with_conn(|c| {
            c.query_row(
                "SELECT updated_at FROM app_config WHERE key = ?1",
                params![TIMEOUTS_KEY],
                |r| r.get(0),
            )
            .unwrap()
        });
        assert_eq!(updated_at, 2);
    }

    #[test]
    fn load_returns_none_on_corrupt_json() {
        // Defensive: a corrupt row must NOT crash the loader; it
        // should log a warning and return Ok(None) so the server
        // boots with the TOML defaults.
        let dir = tempdir();
        let pool = DbPool::open(&dir.join("corrupt.db")).unwrap();
        {
            let mut w = pool.writer();
            crate::db::migrations::run(&mut w).unwrap();
            // Bypass the helper to plant garbage.
            w.execute(
                "INSERT INTO app_config (key, value, updated_at) VALUES (?1, ?2, ?3)",
                params![TIMEOUTS_KEY, "this is not json", 1_i64],
            )
            .unwrap();
        }
        let got = {
            let w = pool.writer();
            load_timeouts_override_from_db(&w).unwrap()
        };
        assert_eq!(got, None, "corrupt JSON must fall back to None");
    }
}
