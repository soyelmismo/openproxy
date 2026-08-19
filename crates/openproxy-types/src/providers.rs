use crate::capabilities::ModelCapabilities;
use crate::ids::ModelId;
use crate::message::TargetFormat;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredModel {
    pub model_id: ModelId,
    pub display_name: Option<String>,
    pub target_format: TargetFormat,
    pub context_length: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub input_modalities: Option<Vec<String>>,
    pub output_modalities: Option<Vec<String>>,
    pub model_type: Option<String>,
    pub family: Option<String>,
    pub capabilities: Option<ModelCapabilities>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderMetadata {
    pub built_in: bool,
    pub deletable: bool,
    pub supports_quota: bool,
    pub quota_refresh_supported: bool,
    pub requires_oauth: bool,
    pub oauth_refresh_lead_seconds: Option<u64>,
}

impl ProviderMetadata {
    pub fn custom_default() -> Self {
        Self {
            built_in: false,
            deletable: true,
            supports_quota: false,
            quota_refresh_supported: false,
            requires_oauth: false,
            oauth_refresh_lead_seconds: None,
        }
    }
}

impl_string_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum ProviderFormat {
        Openai => "openai",
        Anthropic => "anthropic",
        Mixed => "mixed",
        Gemini => "gemini",
        Responses => "responses",
        Atomesus => "atomesus",
    }
    error: "provider format"
}

impl ProviderFormat {
    /// Return the default target format for this provider format.
    #[inline]
    pub const fn default_target_format(&self) -> TargetFormat {
        match self {
            Self::Anthropic => TargetFormat::Anthropic,
            Self::Gemini => TargetFormat::Gemini,
            Self::Responses => TargetFormat::Responses,
            Self::Atomesus => TargetFormat::Atomesus,
            Self::Openai | Self::Mixed => TargetFormat::Openai,
        }
    }
}

impl_string_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum AuthType {
        Bearer => "bearer",
        XApiKey => "x-api-key",
        GoogApiKey => "goog-api-key",
        OAuth => "oauth",
        None => "none",
    }
    error: "auth_type"
}

impl_string_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
    #[serde(rename_all = "lowercase")]
    pub enum RateLimitScope {
        #[default]
        Account => "account",
        Model => "model",
    }
    error: "rate_limit_scope"
}

fn default_proxy_rotation_errors() -> String {
    "429,connect_error,timeout".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub id: crate::ids::ProviderId,
    pub name: String,
    pub base_url: String,
    pub auth_type: AuthType,
    pub format: ProviderFormat,
    pub extra_headers_json: Option<String>,
    pub auto_activate_keyword: Option<String>,
    #[serde(default = "default_true")]
    pub active: bool,
    pub created_at: String,
    #[serde(default)]
    pub use_proxies: bool,
    #[serde(default)]
    pub current_proxy_id: Option<String>,
    #[serde(default = "default_proxy_rotation_errors")]
    pub proxy_rotation_errors: String,
    pub rate_limit_scope: RateLimitScope,
    #[serde(default = "default_proxy_rotation_mode")]
    pub proxy_rotation_mode: String,
    /// Cached favicon as a data URI (`data:image/png;base64,...`).
    /// Populated lazily by the discovery scheduler.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub favicon_base64: Option<String>,
}

fn default_proxy_rotation_mode() -> String {
    "global".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_format_as_str() {
        assert_eq!(ProviderFormat::Openai.as_str(), "openai");
        assert_eq!(ProviderFormat::Anthropic.as_str(), "anthropic");
        assert_eq!(ProviderFormat::Mixed.as_str(), "mixed");
        assert_eq!(ProviderFormat::Gemini.as_str(), "gemini");
        assert_eq!(ProviderFormat::Responses.as_str(), "responses");
        assert_eq!(ProviderFormat::Atomesus.as_str(), "atomesus");
    }

    #[test]
    fn test_provider_format_parse() {
        assert_eq!(
            ProviderFormat::parse("openai").unwrap(),
            ProviderFormat::Openai
        );
        assert_eq!(
            ProviderFormat::parse("anthropic").unwrap(),
            ProviderFormat::Anthropic
        );
        assert_eq!(
            ProviderFormat::parse("mixed").unwrap(),
            ProviderFormat::Mixed
        );
        assert_eq!(
            ProviderFormat::parse("gemini").unwrap(),
            ProviderFormat::Gemini
        );
        assert_eq!(
            ProviderFormat::parse("responses").unwrap(),
            ProviderFormat::Responses
        );
        assert_eq!(
            ProviderFormat::parse("atomesus").unwrap(),
            ProviderFormat::Atomesus
        );

        assert!(ProviderFormat::parse("invalid").is_err());
        assert_eq!(
            ProviderFormat::parse("invalid").unwrap_err(),
            "invalid provider format: invalid"
        );
    }

    #[test]
    fn test_provider_format_default_target_format() {
        assert_eq!(
            ProviderFormat::Openai.default_target_format(),
            TargetFormat::Openai
        );
        assert_eq!(
            ProviderFormat::Anthropic.default_target_format(),
            TargetFormat::Anthropic
        );
        assert_eq!(
            ProviderFormat::Gemini.default_target_format(),
            TargetFormat::Gemini
        );
        assert_eq!(
            ProviderFormat::Responses.default_target_format(),
            TargetFormat::Responses
        );
        assert_eq!(
            ProviderFormat::Atomesus.default_target_format(),
            TargetFormat::Atomesus
        );
        assert_eq!(
            ProviderFormat::Mixed.default_target_format(),
            TargetFormat::Openai
        );
    }

    #[test]
    fn test_auth_type_as_str() {
        assert_eq!(AuthType::Bearer.as_str(), "bearer");
        assert_eq!(AuthType::XApiKey.as_str(), "x-api-key");
        assert_eq!(AuthType::GoogApiKey.as_str(), "goog-api-key");
        assert_eq!(AuthType::OAuth.as_str(), "oauth");
        assert_eq!(AuthType::None.as_str(), "none");
    }

    #[test]
    fn test_rate_limit_scope_parse() {
        assert_eq!(
            RateLimitScope::parse("account").unwrap(),
            RateLimitScope::Account
        );
        assert_eq!(
            RateLimitScope::parse("model").unwrap(),
            RateLimitScope::Model
        );
        assert!(RateLimitScope::parse("invalid").is_err());
    }

    #[test]
    fn test_rate_limit_scope_as_str() {
        assert_eq!(RateLimitScope::Account.as_str(), "account");
        assert_eq!(RateLimitScope::Model.as_str(), "model");
    }

    #[test]
    fn test_auth_type_parse() {
        assert_eq!(AuthType::parse("bearer").unwrap(), AuthType::Bearer);
        assert_eq!(AuthType::parse("x-api-key").unwrap(), AuthType::XApiKey);
        assert_eq!(
            AuthType::parse("goog-api-key").unwrap(),
            AuthType::GoogApiKey
        );
        assert_eq!(AuthType::parse("oauth").unwrap(), AuthType::OAuth);
        assert_eq!(AuthType::parse("none").unwrap(), AuthType::None);

        assert!(AuthType::parse("invalid").is_err());
        assert_eq!(
            AuthType::parse("invalid").unwrap_err(),
            "invalid auth_type: invalid"
        );
    }
}
