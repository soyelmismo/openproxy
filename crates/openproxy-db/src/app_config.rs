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

pub const PROXY_TEST_URL_KEY: &str = "proxy_test_url";
pub const PROXY_TEST_URL_DEFAULT: &str = "https://cloudflare.com/cdn-cgi/trace";

fn load_config_val<T: serde::de::DeserializeOwned>(
    conn: &Connection,
    key: &str,
) -> Result<Option<T>> {
    let mut stmt = conn
        .prepare("SELECT value FROM app_config WHERE key = ?1")
        .map_err(crate::error::map_db_error)?;
    let mut rows = stmt
        .query(params![key])
        .map_err(crate::error::map_db_error)?;
    match rows.next() {
        Ok(Some(row)) => {
            let raw: String = row.get(0).map_err(crate::error::map_db_error)?;
            match serde_json::from_str::<T>(&raw) {
                Ok(cfg) => Ok(Some(cfg)),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        key = key,
                        "app_config row exists but JSON is corrupt; ignoring and falling back to default"
                    );
                    Ok(None)
                }
            }
        }
        Ok(None) => Ok(None),
        Err(e) => Err(crate::error::map_db_error_ctx("iterate load_config_val")(e)),
    }
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

pub fn load_proxy_test_url(conn: &Connection) -> Result<String> {
    let mut stmt = conn
        .prepare("SELECT value FROM app_config WHERE key = ?1")
        .map_err(crate::error::map_db_error)?;
    let mut rows = stmt
        .query(params![PROXY_TEST_URL_KEY])
        .map_err(crate::error::map_db_error)?;
    if let Ok(Some(row)) = rows.next() {
        let raw: String = row.get(0).map_err(crate::error::map_db_error)?;
        if let Ok(s) = serde_json::from_str::<String>(&raw) {
            return Ok(s);
        }
        return Ok(raw);
    }
    Ok(PROXY_TEST_URL_DEFAULT.to_string())
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
            .map_or(0, |d| d.as_nanos());
        let dir = base.join(format!("openproxy-appcfg-test-{pid}-{nanos}"));
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
