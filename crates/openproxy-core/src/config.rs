//! Configuration loaded from config.toml with env-var overrides (OPENPROXY_*__*).
//!
//! Mirrors §10 of mvp-spec.md.

use crate::error::{CoreError, Result};
pub use openproxy_types::config::{
    CircuitBreakerConfig, CompressionMode, CooldownConfig, CooldownMode, EncryptionKeySource,
    MaintenanceConfig, PiiConfig, QuotaProtectionConfig, RacingConfig, RetriesConfig, ServerConfig,
    SmartWarmupConfig, StorageConfig, TimeoutsConfig,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Per-target cooldown. A retryable failure (5xx, 429, timeout, connection
/// error, see [`crate::retry::RetryPolicy::is_retryable`]) parks the target in
/// `target_cooldowns` for `cooldown_secs`, and later requests skip it. The
/// in-memory circuit breaker is the account-scoped counterpart; this is the
/// target-scoped, persistent one.
///
/// Precedence: `OPENPROXY_COOLDOWN_SECS` (read by [`AppConfig::load_or_default`]
/// at load time) over `[cooldown] cooldown_secs` in `config.toml`, so a container
/// can flip the value without rewriting the baked-in file.
///
/// Per-combo columns (`cooldown_base_secs`, `cooldown_max_secs`,
/// `cooldown_factor`, migration 000035) take precedence over these defaults; they
/// are the fallback for a combo whose column is `NULL`. The pipeline resolves
/// combo-override vs global default per request, so a change here lands on the
/// next request without a restart.
///
/// - `cooldown_secs` is both the flat window and the exponential `base_secs`, so a
///   pre-migration config keeps working unchanged.
/// - `max_secs` caps the exponential growth, default 3600.
/// - `factor` is the growth factor, default 2.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    pub format: LogFormat,
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            format: LogFormat::Json,
            level: "info".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Json,
    Text,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CompressionConfig {
    /// Compression mode: "off" | "lite" | "rtk"
    #[serde(default)]
    pub mode: CompressionMode,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            mode: CompressionMode::Off,
        }
    }
}

/// Automatic VACUUM plus usage row retention. Defaults let `[storage]` in
/// `config.toml` omit `[storage.maintenance]` entirely:
///
/// ```toml
/// [storage.maintenance]
/// auto_vacuum = true          # default: true
/// vacuum_interval_hours = 6   # default: 6
/// usage_retention_days = 7    # default: 7
/// ```
///
/// `auto_vacuum = false` stops the background task; `POST /admin/api/debug/vacuum`
/// still works. The prune task always runs, since the `usage` table would grow
/// without bound, and `usage_retention_days` sets how old a row must be.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaSyncConfig {
    #[serde(default = "default_quota_sync_enabled")]
    pub enabled: bool,
    #[serde(default = "default_quota_sync_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_quota_sync_delay")]
    pub delay_between_accounts_ms: u64,
}

fn default_quota_sync_enabled() -> bool {
    true
}

fn default_quota_sync_interval() -> u64 {
    3600
}

fn default_quota_sync_delay() -> u64 {
    5000
}

impl Default for QuotaSyncConfig {
    fn default() -> Self {
        Self {
            enabled: default_quota_sync_enabled(),
            interval_secs: default_quota_sync_interval(),
            delay_between_accounts_ms: default_quota_sync_delay(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub racing: RacingConfig,
    #[serde(default)]
    pub timeouts: TimeoutsConfig,
    #[serde(default)]
    pub retries: RetriesConfig,
    #[serde(default)]
    pub circuit_breaker: CircuitBreakerConfig,
    #[serde(default)]
    pub cooldown: CooldownConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub compression: CompressionConfig,
    #[serde(default)]
    pub quota_protection: QuotaProtectionConfig,
    #[serde(default)]
    pub smart_warmup: SmartWarmupConfig,
    #[serde(default)]
    pub quota_sync: QuotaSyncConfig,
    #[serde(default)]
    pub pii: PiiConfig,
}

impl AppConfig {
    /// Load from a TOML file, where `OPENPROXY_<SECTION>__<FIELD>` overrides.
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let contents = std::fs::read_to_string(path.as_ref())
            .map_err(|e| CoreError::Config(format!("read {}: {}", path.as_ref().display(), e)))?;
        let cfg: AppConfig =
            toml::from_str(&contents).map_err(|e| CoreError::Config(format!("parse: {e}")))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Load with defaults when the file is missing.
    ///
    /// Env overrides are applied after the TOML load. Only
    /// Explicit overrides are supported for cooldown, the administrative bind
    /// and SQLite reader count; other namespace entries remain reserved.
    pub fn load_or_default(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let mut cfg = if path.as_ref().exists() {
            Self::load(path)?
        } else {
            AppConfig::default()
        };
        if let Ok(raw) = std::env::var("OPENPROXY_COOLDOWN_SECS") {
            match raw.trim().parse::<u64>() {
                Ok(v) => cfg.cooldown.cooldown_secs = v,
                Err(e) => {
                    return Err(CoreError::Config(format!(
                        "OPENPROXY_COOLDOWN_SECS: invalid u64 '{raw}': {e}"
                    )));
                }
            }
        }
        if let Ok(raw) = std::env::var("OPENPROXY_SERVER__ADMIN_BIND") {
            cfg.server.admin_bind = (!raw.trim().is_empty()).then(|| raw.trim().to_owned());
        }
        if let Ok(raw) = std::env::var("OPENPROXY_STORAGE__READER_COUNT") {
            cfg.storage.reader_count = raw.trim().parse().map_err(|error| {
                CoreError::Config(format!("OPENPROXY_STORAGE__READER_COUNT: {error}"))
            })?;
        }
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.storage.reader_count > 32 {
            return Err(CoreError::Config(
                "storage.reader_count must be between 0 and 32".into(),
            ));
        }
        if let Some(bind) = &self.server.admin_bind
            && (bind.trim().is_empty() || bind == &self.server.bind)
        {
            return Err(CoreError::Config(
                "server.admin_bind must be nonempty and different from server.bind".into(),
            ));
        }
        Ok(())
    }

    /// Expand ~ to home dir in storage.database_path.
    pub fn expanded_database_path(&self) -> PathBuf {
        if self.storage.database_path.starts_with("~/")
            && let Some(home) = dirs_home()
        {
            return PathBuf::from(self.storage.database_path.replacen(
                "~/",
                &format!("{home}/"),
                1,
            ));
        }
        PathBuf::from(&self.storage.database_path)
    }
}

fn dirs_home() -> Option<String> {
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.racing.max_race_size, 8);
        assert_eq!(cfg.timeouts.idle_chunk_ms, 120_000);
        assert_eq!(cfg.retries.max_attempts, 3);
        assert_eq!(cfg.server.admin_bind, None);
        assert_eq!(cfg.storage.reader_count, 0);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn separated_admin_and_reader_limits_are_validated() {
        let mut cfg = AppConfig::default();
        cfg.server.admin_bind = Some("127.0.0.1:8788".into());
        cfg.storage.reader_count = 4;
        assert!(cfg.validate().is_ok());
        cfg.storage.reader_count = 33;
        assert!(cfg.validate().is_err());
        cfg.storage.reader_count = 0;
        cfg.server.admin_bind = Some(cfg.server.bind.clone());
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn load_example_config() {
        let path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config.example.toml");
        let cfg = AppConfig::load(&path).expect("config.example.toml must load");
        assert_eq!(cfg.racing.default_race_size, 1);
        assert_eq!(cfg.timeouts.ttft_ms, 6_000);
    }

    #[test]
    fn expand_home_dir() {
        let cfg = AppConfig::default();
        let p = cfg.expanded_database_path();
        if let Ok(home) = std::env::var("HOME")
            && cfg.storage.database_path.starts_with("~/")
        {
            assert!(
                p.starts_with(&home),
                "expected to start with home dir, got {p:?}"
            );
        }
    }
}
