//! Configuration loaded from config.toml with env-var overrides (OPENPROXY_*__*).
//!
//! Mirrors §10 of mvp-spec.md.

use crate::compression::CompressionMode;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use crate::error::{CoreError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub bind: String,
    pub request_max_body_bytes: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        // NOTE: Default is 0.0.0.0 to make local dev / docker easy.
        // In production, override via config.toml or OPENPROXY_SERVER__BIND=127.0.0.1:8787
        // (use 127.0.0.1 in production to avoid exposing the admin API).
        Self { bind: "0.0.0.0:8787".into(), request_max_body_bytes: 10 * 1024 * 1024 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub database_path: String,
    pub encryption_key_source: EncryptionKeySource,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            database_path: "~/.openproxy/data.db".into(),
            encryption_key_source: EncryptionKeySource::Env,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EncryptionKeySource {
    Env,
    File,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RacingConfig {
    pub default_race_size: u8,
    pub max_race_size: u8,
    pub abort_grace_ms: u64,
}

impl Default for RacingConfig {
    fn default() -> Self {
        Self { default_race_size: 1, max_race_size: 8, abort_grace_ms: 500 }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimeoutsConfig {
    pub connect_ms: u64,
    pub request_send_ms: u64,
    pub ttft_ms: u64,
    pub idle_chunk_ms: u64,
    pub total_ms: u64,
}

impl Default for TimeoutsConfig {
    fn default() -> Self {
        Self {
            connect_ms: 5_000,
            request_send_ms: 10_000,
            ttft_ms: 30_000,
            idle_chunk_ms: 120_000,
            total_ms: 300_000,
        }
    }
}

/// Default for `idle_chunk_retryable`: false = idle_chunk timeouts
/// return an error and abort the target walk (current behavior).
/// true = idle_chunk timeouts are treated as retryable and the
/// pipeline falls through to the next target.
pub const IDLE_CHUNK_RETRYABLE_DEFAULT: bool = false;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RetriesConfig {
    pub max_attempts: u8,
    pub backoff_base_ms: u64,
    pub backoff_factor: u8,
    pub backoff_jitter_pct: u8,
    #[serde(default = "default_idle_chunk_retryable")]
    pub idle_chunk_retryable: bool,
    /// How many times to retry the entire combo walk if all targets
    /// fail. 1 = no combo-level retry (current behavior). Each retry
    /// re-resolves targets (cooldowns/CB may have changed) and walks
    /// them fresh.
    #[serde(default = "default_combo_max_attempts")]
    pub combo_max_attempts: u8,
}

fn default_idle_chunk_retryable() -> bool {
    IDLE_CHUNK_RETRYABLE_DEFAULT
}

fn default_combo_max_attempts() -> u8 {
    1
}

impl Default for RetriesConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            backoff_base_ms: 200,
            backoff_factor: 2,
            backoff_jitter_pct: 50,
            idle_chunk_retryable: IDLE_CHUNK_RETRYABLE_DEFAULT,
            combo_max_attempts: 1,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CircuitBreakerConfig {
    pub failure_threshold: u8,
    pub unhealthy_duration_ms: u64,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self { failure_threshold: 5, unhealthy_duration_ms: 60_000 }
    }
}

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
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CooldownConfig {
    /// Flat-window duration (seconds) for `cooldown_mode = Flat`,
    /// AND the `base_secs` for `cooldown_mode = Exponential`. The
    /// two cases share a field so a pre-migration config keeps
    /// working unchanged — only the *interpretation* of the value
    /// depends on the combo's `cooldown_mode`.
    pub cooldown_secs: u64,
    /// Cap on the exponential cooldown (seconds). Default 3600.
    /// Only meaningful when `cooldown_mode = Exponential`; ignored
    /// for `Flat` (the flat window is always exactly `cooldown_secs`).
    #[serde(default = "default_cooldown_max_secs")]
    pub max_secs: u64,
    /// Exponential growth factor. Default 2 (each failure doubles
    /// the window). Only meaningful when
    /// `cooldown_mode = Exponential`.
    #[serde(default = "default_cooldown_factor")]
    pub factor: u32,
}

fn default_cooldown_max_secs() -> u64 {
    3600
}

fn default_cooldown_factor() -> u32 {
    2
}

impl Default for CooldownConfig {
    fn default() -> Self {
        Self {
            cooldown_secs: 60,
            max_secs: default_cooldown_max_secs(),
            factor: default_cooldown_factor(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    pub format: LogFormat,
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self { format: LogFormat::Json, level: "info".into() }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat { Json, Text }

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
}

impl AppConfig {
    /// Load from a TOML file. Env vars OPENPROXY_<SECTION>__<FIELD> override.
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let contents = std::fs::read_to_string(path.as_ref())
            .map_err(|e| CoreError::Config(format!("read {}: {}", path.as_ref().display(), e)))?;
        let cfg: AppConfig = toml::from_str(&contents)
            .map_err(|e| CoreError::Config(format!("parse: {}", e)))?;
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
        if self.storage.database_path.starts_with("~/") {
            if let Some(home) = dirs_home() {
                return PathBuf::from(self.storage.database_path.replacen("~/", &format!("{}/", home), 1));
            }
        }
        PathBuf::from(&self.storage.database_path)
    }
}

fn dirs_home() -> Option<String> {
    std::env::var("HOME").ok().or_else(|| std::env::var("USERPROFILE").ok())
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
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config.example.toml");
        let cfg = AppConfig::load(&path).expect("config.example.toml must load");
        assert_eq!(cfg.racing.default_race_size, 1);
        assert_eq!(cfg.timeouts.ttft_ms, 30_000);
    }

    #[test]
    fn expand_home_dir() {
        let cfg = AppConfig::default();
        let p = cfg.expanded_database_path();
        if let Ok(home) = std::env::var("HOME") {
            if cfg.storage.database_path.starts_with("~/") {
                assert!(p.starts_with(&home), "expected to start with home dir, got {:?}", p);
            }
        }
    }
}
