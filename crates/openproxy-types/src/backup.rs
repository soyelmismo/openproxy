use crate::combos::{PriorityMode, Strategy};
use crate::config::CooldownMode;
use crate::ids::{ModelId, ProviderId};
use crate::message::TargetFormat;
use crate::providers::{AuthType, ProviderFormat, RateLimitScope};
use serde::{Deserialize, Serialize};

pub const BACKUP_FORMAT_VERSION: u32 = 1;

fn default_true() -> bool {
    true
}

fn default_priority() -> i32 {
    100
}

fn default_health_status() -> String {
    "healthy".into()
}

fn default_model_type() -> String {
    "chat".into()
}

fn default_race_size() -> u8 {
    1
}

fn default_weight() -> i32 {
    1
}

fn default_proxy_rotation_errors_str() -> String {
    "403,429".into()
}

fn default_proxy_rotation_mode_str() -> String {
    "global".into()
}

fn default_scopes_json() -> String {
    "[\"chat\"]".into()
}

/// Self-contained backup bundle for OpenProxy state and configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackupBundle {
    pub version: u32,
    pub exported_at: String,
    pub openproxy_version: String,
    #[serde(default)]
    pub encrypted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kdf: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kdf_salt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kdf_iterations: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ciphertext: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<BackupPayload>,
}

/// Decrypted/plain payload containing database entities and configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BackupPayload {
    #[serde(default)]
    pub providers: Vec<BackupProvider>,
    #[serde(default)]
    pub accounts: Vec<BackupAccount>,
    #[serde(default)]
    pub models: Vec<BackupModel>,
    #[serde(default)]
    pub combos: Vec<BackupCombo>,
    #[serde(default)]
    pub combo_targets: Vec<BackupComboTarget>,
    #[serde(default)]
    pub proxy_sources: Vec<BackupProxySource>,
    #[serde(default)]
    pub api_keys: Vec<BackupApiKey>,
    #[serde(default)]
    pub app_config: Vec<BackupAppConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupProvider {
    pub id: ProviderId,
    pub name: String,
    pub base_url: String,
    pub auth_type: AuthType,
    pub format: ProviderFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_headers_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_activate_keyword: Option<String>,
    #[serde(default = "default_true")]
    pub active: bool,
    #[serde(default)]
    pub use_proxies: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_proxy_id: Option<String>,
    #[serde(default = "default_proxy_rotation_errors_str")]
    pub proxy_rotation_errors: String,
    #[serde(default)]
    pub rate_limit_scope: RateLimitScope,
    #[serde(default)]
    pub notif_keyword_only: bool,
    #[serde(default = "default_proxy_rotation_mode_str")]
    pub proxy_rotation_mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupAccount {
    pub id: i64,
    pub provider_id: ProviderId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default = "default_priority")]
    pub priority: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_config_json: Option<String>,
    #[serde(default = "default_health_status")]
    pub health_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limited_until: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_provider_specific: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_proxy_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupModel {
    pub id: i64,
    pub provider_id: ProviderId,
    pub model_id: ModelId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub target_format: TargetFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_overrides_json: Option<String>,
    #[serde(default = "default_true")]
    pub active: bool,
    #[serde(default)]
    pub custom: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(default = "default_model_type")]
    pub model_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_modalities_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_modalities_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manually_disabled_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackupCombo {
    pub id: i64,
    pub name: String,
    pub strategy: Strategy,
    #[serde(default = "default_race_size")]
    pub race_size: u8,
    #[serde(default)]
    pub preventive_rate_limit: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<i64>,
    #[serde(default)]
    pub priority_mode: PriorityMode,
    #[serde(default)]
    pub cooldown_mode: CooldownMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_base_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_max_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_factor: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lkgp_exploration_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_window_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupComboTarget {
    pub id: i64,
    pub combo_id: i64,
    pub provider_id: ProviderId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_row_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_combo_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_model_id: Option<String>,
    pub priority_order: i32,
    #[serde(default = "default_weight")]
    pub weight: i32,
    #[serde(default = "default_true")]
    pub active: bool,
    #[serde(default)]
    pub rate_limit_scope: RateLimitScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_mode: Option<CooldownMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_base_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_max_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_factor: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupProxySource {
    pub id: String,
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub priority: i32,
    #[serde(default = "default_true")]
    pub active: bool,
    #[serde(default)]
    pub is_builtin: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupApiKey {
    pub id: i64,
    pub key_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default = "default_scopes_json")]
    pub scopes_json: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_models_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_combos_json: Option<String>,
    #[serde(default = "default_true")]
    pub is_active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blacklisted_providers_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blacklisted_models_json: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupAppConfig {
    pub key: String,
    pub value: String,
    #[serde(default)]
    pub updated_at: i64,
}

/// Inspection summary of a backup bundle without altering state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupValidationSummary {
    pub version: u32,
    pub encrypted: bool,
    pub exported_at: String,
    pub openproxy_version: String,
    pub providers_count: usize,
    pub accounts_count: usize,
    pub models_count: usize,
    pub combos_count: usize,
    pub combo_targets_count: usize,
    pub proxy_sources_count: usize,
    pub api_keys_count: usize,
    pub app_config_count: usize,
    pub warnings: Vec<String>,
}

/// Options passed into restore operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RestoreOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

/// Result report produced after restoring a backup bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RestoreReport {
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safety_backup_path: Option<String>,
    pub providers_restored: usize,
    pub accounts_restored: usize,
    pub models_restored: usize,
    pub combos_restored: usize,
    pub combo_targets_restored: usize,
    pub proxy_sources_restored: usize,
    pub api_keys_restored: usize,
    pub app_config_restored: usize,
    pub migrations_applied: usize,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backup_bundle_serde_defaults() {
        let json = r#"{
            "version": 1,
            "exported_at": "2026-03-30T00:00:00Z",
            "openproxy_version": "1.0.0"
        }"#;

        let bundle: BackupBundle = serde_json::from_str(json).expect("failed to deserialize minimal BackupBundle");
        assert_eq!(bundle.version, 1);
        assert_eq!(bundle.exported_at, "2026-03-30T00:00:00Z");
        assert_eq!(bundle.openproxy_version, "1.0.0");
        assert!(!bundle.encrypted);
        assert!(bundle.kdf.is_none());
        assert!(bundle.payload.is_none());
    }

    #[test]
    fn test_restore_report_serde() {
        let report = RestoreReport {
            success: true,
            safety_backup_path: None,
            providers_restored: 1,
            accounts_restored: 2,
            models_restored: 3,
            combos_restored: 4,
            combo_targets_restored: 5,
            proxy_sources_restored: 6,
            api_keys_restored: 7,
            app_config_restored: 8,
            migrations_applied: 0,
            message: "Restore successful".to_string(),
        };

        let json = serde_json::to_string(&report).expect("failed to serialize RestoreReport");
        assert!(!json.contains("safety_backup_path"));

        let deserialized: RestoreReport = serde_json::from_str(&json).expect("failed to deserialize RestoreReport");
        assert_eq!(report, deserialized);
    }
}
