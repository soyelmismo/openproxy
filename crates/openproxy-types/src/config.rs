use serde::{Deserialize, Serialize};

impl_string_enum! {
    #[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
    #[serde(rename_all = "snake_case")]
    pub enum CompressionMode {
        #[default]
        Off => "off",
        Lite => "lite",
        Rtk => "rtk",
        LiteRtk => "lite_rtk",
    }
    error: "compression_mode"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub bind: String,
    pub request_max_body_bytes: usize,
    #[serde(default = "default_rate_limit_requests")]
    pub rate_limit_requests_per_minute: u32,
    #[serde(default = "default_allow_anonymous")]
    pub allow_anonymous: bool,
}

fn default_rate_limit_requests() -> u32 {
    1000
}

fn default_allow_anonymous() -> bool {
    false
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8787".into(),
            request_max_body_bytes: 10 * 1024 * 1024,
            rate_limit_requests_per_minute: 1000,
            allow_anonymous: false,
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

impl_string_enum! {
    #[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "lowercase")]
    pub enum EncryptionKeySource {
        #[default]
        Env => "env",
        File => "file",
    }
    error: "encryption_key_source"
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
            ttft_ms: 6_000,
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
    pub models: Box<[String]>,
}

fn default_smart_warmup_enabled() -> bool {
    true
}
fn default_smart_warmup_interval() -> u64 {
    3600
}

fn default_smart_warmup_models() -> Box<[String]> {
    vec![
        "gemini-3.5-flash-extra-low".to_string(),
        "claude-sonnet-4-6".to_string(),
    ]
    .into_boxed_slice()
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

impl_string_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, Hash)]
    #[serde(rename_all = "snake_case")]
    pub enum CooldownMode {
        #[default]
        Flat => "flat",
        Exponential => "exponential",
        None => "none" | "disabled" | "off",
    }
    error: "cooldown_mode"
}

impl_string_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
    #[serde(rename_all = "snake_case")]
    pub enum PiiEntity {
        Email => "email",
        Phone => "phone",
        #[serde(alias = "ipv4", alias = "ipv6")]
        Ip => "ip" | "ipv4" | "ipv6",
        #[serde(alias = "card")]
        CreditCard => "credit_card" | "card",
        #[serde(alias = "key", alias = "api_key")]
        Secret => "secret" | "key" | "api_key",
        #[serde(alias = "name")]
        Person => "person" | "name",
    }
    core_error: "pii_entity"
}

impl PiiEntity {
    pub const ALL: [Self; 6] = [
        Self::Email,
        Self::Phone,
        Self::Ip,
        Self::CreditCard,
        Self::Secret,
        Self::Person,
    ];

    #[must_use]
    pub const fn placeholder_prefix(self) -> &'static str {
        match self {
            Self::Email => "EMAIL",
            Self::Phone => "PHONE",
            Self::Ip => "IP",
            Self::CreditCard => "CARD",
            Self::Secret => "KEY",
            Self::Person => "PERSON",
        }
    }
}

fn deserialize_dedup_entities<'de, D>(deserializer: D) -> Result<Vec<PiiEntity>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let list = Vec::<PiiEntity>::deserialize(deserializer)?;
    let mut seen = std::collections::HashSet::with_capacity(list.len());
    let mut out = Vec::with_capacity(list.len());
    for item in list {
        if seen.insert(item) {
            out.push(item);
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiiConfig {
    #[serde(default = "default_pii_enabled", alias = "enabled")]
    pub pii_enabled: bool,
    #[serde(default = "default_pii_reversible", alias = "reversible")]
    pub pii_reversible: bool,
    #[serde(default = "default_pii_redact_logs", alias = "redact_logs")]
    pub pii_redact_logs: bool,
    #[serde(
        default = "default_pii_entities",
        alias = "entities",
        deserialize_with = "deserialize_dedup_entities"
    )]
    pub pii_entities: Vec<PiiEntity>,
}

fn default_pii_enabled() -> bool {
    false
}

fn default_pii_reversible() -> bool {
    true
}

fn default_pii_redact_logs() -> bool {
    true
}

fn default_pii_entities() -> Vec<PiiEntity> {
    PiiEntity::ALL.to_vec()
}

impl Default for PiiConfig {
    fn default() -> Self {
        Self {
            pii_enabled: default_pii_enabled(),
            pii_reversible: default_pii_reversible(),
            pii_redact_logs: default_pii_redact_logs(),
            pii_entities: default_pii_entities(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FromDb;

    #[test]
    fn test_compression_mode_enum() {
        assert_eq!(CompressionMode::Off.as_str(), "off");
        assert_eq!(CompressionMode::Lite.as_str(), "lite");
        assert_eq!(CompressionMode::Rtk.as_str(), "rtk");
        assert_eq!(CompressionMode::LiteRtk.as_str(), "lite_rtk");
        assert_eq!(CompressionMode::parse("lite"), Ok(CompressionMode::Lite));
        assert_eq!(CompressionMode::parse("rtk"), Ok(CompressionMode::Rtk));
        assert!(CompressionMode::parse("invalid").is_err());
    }

    #[test]
    fn test_encryption_key_source_enum() {
        assert_eq!(EncryptionKeySource::Env.as_str(), "env");
        assert_eq!(EncryptionKeySource::File.as_str(), "file");
        assert_eq!(
            EncryptionKeySource::parse("env"),
            Ok(EncryptionKeySource::Env)
        );
        assert_eq!(
            EncryptionKeySource::parse("file"),
            Ok(EncryptionKeySource::File)
        );
        assert!(EncryptionKeySource::parse("invalid").is_err());
    }

    #[test]
    fn test_cooldown_mode_as_str() {
        assert_eq!(CooldownMode::Flat.as_str(), "flat");
        assert_eq!(CooldownMode::Exponential.as_str(), "exponential");
        assert_eq!(CooldownMode::None.as_str(), "none");
    }

    #[test]
    fn test_cooldown_mode_parse() {
        assert_eq!(CooldownMode::parse("flat"), Ok(CooldownMode::Flat));
        assert_eq!(
            CooldownMode::parse("exponential"),
            Ok(CooldownMode::Exponential)
        );
        assert_eq!(CooldownMode::parse("none"), Ok(CooldownMode::None));
        assert_eq!(CooldownMode::parse("disabled"), Ok(CooldownMode::None));
        assert_eq!(CooldownMode::parse("off"), Ok(CooldownMode::None));
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
        assert_eq!(CooldownMode::from_db(Some("none")), CooldownMode::None);
        assert_eq!(CooldownMode::from_db(Some("disabled")), CooldownMode::None);
        assert_eq!(CooldownMode::from_db(Some("off")), CooldownMode::None);
        assert_eq!(CooldownMode::from_db(Some("flat")), CooldownMode::Flat);
        assert_eq!(CooldownMode::from_db(Some("unknown")), CooldownMode::Flat);
        assert_eq!(CooldownMode::from_db(None), CooldownMode::Flat);
    }

    #[test]
    fn test_pii_entity_and_config() {
        assert_eq!(PiiEntity::Email.as_str(), "email");
        assert_eq!(PiiEntity::Email.placeholder_prefix(), "EMAIL");
        assert_eq!(PiiEntity::Phone.placeholder_prefix(), "PHONE");
        assert_eq!(PiiEntity::Ip.placeholder_prefix(), "IP");
        assert_eq!(PiiEntity::CreditCard.placeholder_prefix(), "CARD");
        assert_eq!(PiiEntity::Secret.placeholder_prefix(), "KEY");
        assert_eq!(PiiEntity::Person.placeholder_prefix(), "PERSON");

        assert_eq!(PiiEntity::parse("email").unwrap(), PiiEntity::Email);
        assert_eq!(PiiEntity::parse("card").unwrap(), PiiEntity::CreditCard);
        assert_eq!(
            PiiEntity::parse("credit_card").unwrap(),
            PiiEntity::CreditCard
        );
        assert_eq!(PiiEntity::parse("key").unwrap(), PiiEntity::Secret);
        assert_eq!(PiiEntity::parse("api_key").unwrap(), PiiEntity::Secret);

        let default_cfg = PiiConfig::default();
        assert!(!default_cfg.pii_enabled);
        assert!(default_cfg.pii_reversible);
        assert!(default_cfg.pii_redact_logs);
        assert_eq!(default_cfg.pii_entities.len(), 6);

        let json = serde_json::to_string(&default_cfg).unwrap();
        let parsed: PiiConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(default_cfg, parsed);

        // Test aliases
        let aliased_json = r#"{"enabled": true, "reversible": false, "redact_logs": false, "entities": ["email", "key"]}"#;
        let aliased: PiiConfig = serde_json::from_str(aliased_json).unwrap();
        assert!(aliased.pii_enabled);
        assert!(!aliased.pii_reversible);
        assert!(!aliased.pii_redact_logs);
        assert_eq!(
            aliased.pii_entities,
            vec![PiiEntity::Email, PiiEntity::Secret]
        );

        // Test deduplication of repeated and aliased entities
        let dup_json = r#"{"pii_entities": ["credit_card", "person", "card", "person", "credit_card", "name"]}"#;
        let dup_cfg: PiiConfig = serde_json::from_str(dup_json).unwrap();
        assert_eq!(
            dup_cfg.pii_entities,
            vec![PiiEntity::CreditCard, PiiEntity::Person]
        );
    }
}
