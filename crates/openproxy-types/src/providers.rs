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

pub fn builtin_provider_ids() -> &'static [&'static str] {
    &[
        "openrouter",
        "minimax",
        "opencode-zen",
        "ollama-cloud",
        "nous-research",
        "nvidia-nim",
        "kilocode",
        "gemini",
        "antigravity",
        "codex",
        "kiro",
        "cloudflare-workers-ai",
    ]
}

pub fn is_builtin(id: &str) -> bool {
    builtin_provider_ids().contains(&id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderFormat {
    Openai,
    Anthropic,
    Mixed,
    Gemini,
    Responses,
}

impl ProviderFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderFormat::Openai => "openai",
            ProviderFormat::Anthropic => "anthropic",
            ProviderFormat::Mixed => "mixed",
            ProviderFormat::Gemini => "gemini",
            ProviderFormat::Responses => "responses",
        }
    }

    pub fn parse(s: &str) -> std::result::Result<Self, String> {
        match s {
            "openai" => Ok(ProviderFormat::Openai),
            "anthropic" => Ok(ProviderFormat::Anthropic),
            "mixed" => Ok(ProviderFormat::Mixed),
            "gemini" => Ok(ProviderFormat::Gemini),
            "responses" => Ok(ProviderFormat::Responses),
            other => Err(format!("invalid provider format: {}", other)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthType {
    Bearer,
    XApiKey,
    GoogApiKey,
    OAuth,
    None,
}

impl AuthType {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthType::Bearer => "bearer",
            AuthType::XApiKey => "x-api-key",
            AuthType::GoogApiKey => "goog-api-key",
            AuthType::OAuth => "oauth",
            AuthType::None => "none",
        }
    }

    pub fn parse(s: &str) -> std::result::Result<Self, String> {
        match s {
            "bearer" => Ok(AuthType::Bearer),
            "x-api-key" => Ok(AuthType::XApiKey),
            "goog-api-key" => Ok(AuthType::GoogApiKey),
            "oauth" => Ok(AuthType::OAuth),
            "none" => Ok(AuthType::None),
            other => Err(format!("invalid auth_type: {}", other)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitScope {
    #[default]
    Account,
    Model,
}

impl RateLimitScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            RateLimitScope::Account => "account",
            RateLimitScope::Model => "model",
        }
    }

    pub fn parse(s: &str) -> std::result::Result<Self, String> {
        match s {
            "account" => Ok(RateLimitScope::Account),
            "model" => Ok(RateLimitScope::Model),
            other => Err(format!("invalid rate_limit_scope: {}", other)),
        }
    }
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
    }

    #[test]
    fn test_provider_format_parse() {
        assert_eq!(ProviderFormat::parse("openai").unwrap(), ProviderFormat::Openai);
        assert_eq!(ProviderFormat::parse("anthropic").unwrap(), ProviderFormat::Anthropic);
        assert_eq!(ProviderFormat::parse("mixed").unwrap(), ProviderFormat::Mixed);
        assert_eq!(ProviderFormat::parse("gemini").unwrap(), ProviderFormat::Gemini);
        assert_eq!(ProviderFormat::parse("responses").unwrap(), ProviderFormat::Responses);

        assert!(ProviderFormat::parse("invalid").is_err());
        assert_eq!(
            ProviderFormat::parse("invalid").unwrap_err(),
            "invalid provider format: invalid"
        );
    }
}
