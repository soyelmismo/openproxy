//! Configuration loaded from config.toml with env-var overrides (OPENPROXY_*__*).
//!
//! Mirrors §10 of mvp-spec.md.

use crate::error::{CoreError, Result};
pub use openproxy_types::config::{
    CircuitBreakerConfig, CompressionMode, CooldownConfig, CooldownMode, EncryptionKeySource,
    MaintenanceConfig, QuotaProtectionConfig, RacingConfig, RetriesConfig, ServerConfig,
    SmartWarmupConfig, StorageConfig, TimeoutsConfig,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Per-target cooldown duration. When a target fails with a
/// retryable error (5xx, 429, timeout, or connection error — see
/// [`crate::retry::RetryPolicy::is_retryable`]), the pipeline parks
/// it in `target_cooldowns` for `cooldown_secs` seconds and skips
/// it on subsequent requests. The in-memory circuit breaker
/// complements this for *accounts*; this section is the
/// *target*-scoped, *persistent* counterpart.
///
/// Override at the boundary:
/// - `OPENPROXY_COOLDOWN_SECS` env var (read at config-load time,
///   wins over the TOML value)
/// - `[cooldown] cooldown_secs = 60` in `config.toml`
///
/// The env var is checked in [`AppConfig::load_or_default`] so a
/// docker container can flip the cooldown without rewriting the
/// baked-in config file.
///
/// ## Exponential cooldown (migration 000035)
///
/// Per-combo overrides (`cooldown_base_secs`, `cooldown_max_secs`,
/// `cooldown_factor`) on the `combos` table take precedence over
/// these defaults; the fields below are the fallback used when a
/// combo's column is `NULL` (the legacy / pre-migration-000035
/// state). The pipeline resolves "combo override or global
/// default?" at request time, so flipping a value here takes
/// effect on the next request without a restart.
///
/// - `cooldown_secs`: the flat-window duration AND the exponential
///   `base_secs` (the two are intentionally the same field so a
///   pre-migration config keeps working unchanged).
/// - `max_secs`: the cap on the exponential growth. Default 3600
///   (1 hour).
/// - `factor`: the exponential growth factor. Default 2 (each
///   failure doubles the cooldown window).

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
    /// Modo de compresión: "off" | "lite" | "rtk"
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

/// Database maintenance configuration: automatic VACUUM + usage row
/// retention. All fields have sensible defaults so the `[storage]`
/// section in `config.toml` can omit the entire `[storage.maintenance]`
/// subsection.
///
/// ## TOML example
///
/// ```toml
/// [storage.maintenance]
/// auto_vacuum = true          # default: true
/// vacuum_interval_hours = 6   # default: 6
/// usage_retention_days = 7    # default: 7
/// ```
///
/// Set `auto_vacuum = false` to disable the background VACUUM task
/// entirely (the manual `POST /admin/api/debug/vacuum` endpoint still
/// works). The usage row prune task runs regardless (it prevents the
/// `usage` table from growing without bound), but `usage_retention_days`
/// controls how old rows must be before they're deleted.

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
}

impl AppConfig {
    /// Load from a TOML file. Env vars OPENPROXY_<SECTION>__<FIELD> override.
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let contents = std::fs::read_to_string(path.as_ref())
            .map_err(|e| CoreError::Config(format!("read {}: {}", path.as_ref().display(), e)))?;
        let cfg: AppConfig =
            toml::from_str(&contents).map_err(|e| CoreError::Config(format!("parse: {}", e)))?;
        Ok(cfg)
    }

    /// Load with default fallback if file doesn't exist.
    ///
    /// Env-var overrides (per the spec's `OPENPROXY_*` convention) are
    /// applied *after* the TOML load. Today we only honor
    /// `OPENPROXY_COOLDOWN_SECS` (the only knob that operators
    /// typically want to flip without rewriting the config file);
    /// the rest of the `OPENPROXY_*__*` namespace is reserved for a
    /// future structured-override pass.
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
                        "OPENPROXY_COOLDOWN_SECS: invalid u64 '{}': {}",
                        raw, e
                    )));
                }
            }
        }
        Ok(cfg)
    }

    /// Expand ~ to home dir in storage.database_path.
    pub fn expanded_database_path(&self) -> PathBuf {
        if self.storage.database_path.starts_with("~/")
            && let Some(home) = dirs_home()
        {
            return PathBuf::from(self.storage.database_path.replacen(
                "~/",
                &format!("{}/", home),
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
    }

    #[test]
    fn load_example_config() {
        let path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config.example.toml");
        let cfg = AppConfig::load(&path).expect("config.example.toml must load");
        assert_eq!(cfg.racing.default_race_size, 1);
        assert_eq!(cfg.timeouts.ttft_ms, 30_000);
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
                "expected to start with home dir, got {:?}",
                p
            );
        }
    }
}
