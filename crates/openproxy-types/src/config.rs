use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CompressionMode {
    #[default]
    Off,
    Lite,
    Rtk,
    LiteRtk,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub bind: String,
    pub request_max_body_bytes: usize,
    #[serde(default = "default_rate_limit_requests")]
    pub rate_limit_requests_per_minute: u32,
}

fn default_rate_limit_requests() -> u32 {
    1000
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8787".into(),
            request_max_body_bytes: 10 * 1024 * 1024,
            rate_limit_requests_per_minute: 1000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub database_path: String,
    pub encryption_key_source: EncryptionKeySource,
    #[serde(default)]
    pub maintenance: MaintenanceConfig,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            database_path: "~/.openproxy/data.db".into(),
            encryption_key_source: EncryptionKeySource::Env,
            maintenance: MaintenanceConfig::default(),
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
pub struct MaintenanceConfig {
    #[serde(default = "default_auto_vacuum")]
    pub auto_vacuum: bool,
    #[serde(default = "default_maintenance_interval_secs")]
    pub interval_secs: u64,
    #[serde(default = "default_usage_retention_days")]
    pub usage_retention_days: u32,
    #[serde(default = "default_recording_ttl_secs")]
    pub recording_ttl_secs: i64,
}

fn default_auto_vacuum() -> bool {
    true
}
fn default_maintenance_interval_secs() -> u64 {
    6 * 3600
}
fn default_usage_retention_days() -> u32 {
    7
}
fn default_recording_ttl_secs() -> i64 {
    300
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self {
            auto_vacuum: default_auto_vacuum(),
            interval_secs: default_maintenance_interval_secs(),
            usage_retention_days: default_usage_retention_days(),
            recording_ttl_secs: default_recording_ttl_secs(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RacingConfig {
    pub default_race_size: u8,
    pub max_race_size: u8,
    pub abort_grace_ms: u64,
}

impl Default for RacingConfig {
    fn default() -> Self {
        Self {
            default_race_size: 1,
            max_race_size: 8,
            abort_grace_ms: 500,
        }
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

pub const IDLE_CHUNK_RETRYABLE_DEFAULT: bool = false;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RetriesConfig {
    pub max_attempts: u8,
    pub backoff_base_ms: u64,
    pub backoff_factor: u8,
    pub backoff_jitter_pct: u8,
    #[serde(default = "default_idle_chunk_retryable")]
    pub idle_chunk_retryable: bool,
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
        Self {
            failure_threshold: 5,
            unhealthy_duration_ms: 60_000,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CooldownConfig {
    pub cooldown_secs: u64,
    #[serde(default = "default_cooldown_max_secs")]
    pub max_secs: u64,
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
pub struct QuotaProtectionConfig {
    #[serde(default = "default_quota_protection_enabled")]
    pub enabled: bool,
    #[serde(default = "default_quota_protection_threshold")]
    pub threshold_percentage: u32,
}

fn default_quota_protection_enabled() -> bool {
    true
}
fn default_quota_protection_threshold() -> u32 {
    10
}

impl Default for QuotaProtectionConfig {
    fn default() -> Self {
        Self {
            enabled: default_quota_protection_enabled(),
            threshold_percentage: default_quota_protection_threshold(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartWarmupConfig {
    #[serde(default = "default_smart_warmup_enabled")]
    pub enabled: bool,
    #[serde(default = "default_smart_warmup_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_smart_warmup_models")]
    pub models: Vec<String>,
}

fn default_smart_warmup_enabled() -> bool {
    true
}
fn default_smart_warmup_interval() -> u64 {
    3600
}

fn default_smart_warmup_models() -> Vec<String> {
    vec![
        "gemini-3.5-flash-extra-low".to_string(),
        "claude-sonnet-4-6".to_string(),
    ]
}

impl Default for SmartWarmupConfig {
    fn default() -> Self {
        Self {
            enabled: default_smart_warmup_enabled(),
            interval_secs: default_smart_warmup_interval(),
            models: default_smart_warmup_models(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CooldownMode {
    #[default]
    Flat,
    Exponential,
}

impl CooldownMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Flat => "flat",
            Self::Exponential => "exponential",
        }
    }
    pub fn parse(s: &str) -> std::result::Result<Self, String> {
        match s {
            "flat" => Ok(Self::Flat),
            "exponential" => Ok(Self::Exponential),
            other => Err(format!("invalid cooldown_mode: {}", other)),
        }
    }
    pub fn from_db(s: Option<&str>) -> Self {
        match s {
            Some("exponential") => Self::Exponential,
            _ => Self::Flat,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cooldown_mode_as_str() {
        assert_eq!(CooldownMode::Flat.as_str(), "flat");
        assert_eq!(CooldownMode::Exponential.as_str(), "exponential");
    }

    #[test]
    fn test_cooldown_mode_parse() {
        assert_eq!(CooldownMode::parse("flat"), Ok(CooldownMode::Flat));
        assert_eq!(
            CooldownMode::parse("exponential"),
            Ok(CooldownMode::Exponential)
        );
        assert!(CooldownMode::parse("unknown").is_err());
        assert_eq!(
            CooldownMode::parse("invalid"),
            Err("invalid cooldown_mode: invalid".to_string())
        );
    }

    #[test]
    fn test_cooldown_mode_from_db() {
        assert_eq!(
            CooldownMode::from_db(Some("exponential")),
            CooldownMode::Exponential
        );
        assert_eq!(CooldownMode::from_db(Some("flat")), CooldownMode::Flat);
        assert_eq!(CooldownMode::from_db(Some("unknown")), CooldownMode::Flat);
        assert_eq!(CooldownMode::from_db(None), CooldownMode::Flat);
    }
}
