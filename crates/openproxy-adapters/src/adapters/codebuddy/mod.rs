use super::{
    AdapterAuthType, AdapterFormat, Arc, DiscoveredModel, ModelId, ProviderAdapter,
    ProviderAdapterConfig, ProviderId, Result, TargetFormat, UpstreamClient, UpstreamRequest,
};

use crate::client_contract::Snapshot;
use crate::client_contract::providers::codebuddy_runtime::CodeBuddyProfile;
use crate::client_contract::providers::codebuddy_state;

pub mod models;
pub mod quota;
#[cfg(test)]
mod tests;

pub use models::{
    CODEBUDDY_MODELS, CodeBuddyModelDef, codebuddy_static_models, is_codebuddy_model_free,
    make_codebuddy_model_id, normalize_codebuddy_model_name, parse_codebuddy_credits_multiplier,
};

pub use crate::spoofer::{
    CODEBUDDY_SPOOFING_HEADERS, DEFAULT_CODEBUDDY_VERSION, current_codebuddy_ua,
    current_codebuddy_version, reset_dynamic_codebuddy_overrides,
    set_dynamic_codebuddy_extra_header, set_dynamic_codebuddy_ua, set_dynamic_codebuddy_version,
};
use crate::spoofer::{ClientSpoofer, CodeBuddySpoofer};
pub use quota::{
    CODEBUDDY_ACCOUNTS_URL, CODEBUDDY_GET_USER_RESOURCE_SUMMARY_URL,
    CODEBUDDY_GET_USER_RESOURCE_URL, CODEBUDDY_MODEL_CREDIT_COSTS, CodeBuddyModelCreditCost,
    DEFAULT_CODEBUDDY_BASE_URL, MIRROR_CODEBUDDY_BASE_URL, build_codebuddy_accounts_request,
    build_codebuddy_accounts_request_with_url, build_codebuddy_quota_model_details,
    build_codebuddy_resource_request, calculate_next_midnight_cst_unix_secs, codebuddy_base_url,
    codebuddy_candidate_origins, codebuddy_origin_from_base_url, fetch_codebuddy_quota_unified,
    is_codebuddy_auth_error, parse_codebuddy_accounts_quota, parse_codebuddy_provider_specific,
    parse_codebuddy_resource_quota, parse_codebuddy_resource_quota_at,
    parse_cst_datetime_to_unix_secs,
};

pub fn apply_codebuddy_spoofing_headers(req: &mut UpstreamRequest) {
    CodeBuddySpoofer.apply_to_request(req);
}

pub const NPM_CODEBUDDY_METADATA_URL: &str =
    "https://registry.npmjs.org/@tencent-ai/codebuddy-code/latest";

pub fn codebuddy_npm_metadata_url() -> String {
    std::env::var("OPENPROXY_CODEBUDDY_NPM_METADATA_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| NPM_CODEBUDDY_METADATA_URL.to_string())
}

pub fn get_codebuddy_version() -> String {
    current_codebuddy_version()
}

pub fn set_codebuddy_version(version: String) {
    set_dynamic_codebuddy_version(version);
}

pub fn get_codebuddy_ua() -> String {
    current_codebuddy_ua()
}

pub fn set_codebuddy_ua(ua: impl Into<String>) {
    set_dynamic_codebuddy_ua(ua);
}

/// Actualizar la versión anunciada del CLI CodeBuddy.
///
/// Si la validación de contrato por paquete→commit falla (p. ej. porque el paquete
/// npm no expone `gitHead`), consulta de forma fallback la metadata oficial del registro
/// npm de `@tencent-ai/codebuddy-code` y actualiza en caliente los headers dinámicos
/// (`X-IDE-Version`, `User-Agent`) en memoria.
pub async fn refresh_codebuddy_version(upstream_client: &Arc<UpstreamClient>) -> Option<String> {
    if let Some(version) =
        crate::client_contract::providers::codebuddy_state::refresh_validated(upstream_client).await
    {
        set_dynamic_codebuddy_version(version.clone());
        return Some(version);
    }

    // Fallback dinámico de versión directo desde npm registry metadata
    let url = crate::client_contract::providers::codebuddy_runtime::CODEBUDDY_REGISTRY_LATEST_URL;
    let mut req = crate::upstream::UpstreamRequest::get(url);
    if let Ok(accept) = http::HeaderValue::from_str("application/json") {
        req.headers.insert(http::header::ACCEPT, accept);
    }
    if let Ok(ua) = http::HeaderValue::from_str(&current_codebuddy_ua()) {
        req.headers.insert(http::header::USER_AGENT, ua);
    }

    let cancel = crate::upstream::CancellationToken::new();
    let resp = upstream_client
        .call(req, crate::upstream::TimeoutProfile::OAuth, cancel)
        .await
        .ok()?;

    if !resp.status.is_success() {
        return None;
    }

    let body = resp.collect().await.ok()?;
    let json: serde_json::Value = serde_json::from_slice(&body).ok()?;
    let raw_ver = json.get("version").and_then(|v| v.as_str())?;
    let clean_ver = raw_ver
        .trim()
        .trim_start_matches('v')
        .split('-')
        .next()?
        .trim();

    if !clean_ver.is_empty() {
        set_dynamic_codebuddy_version(clean_ver);
        return Some(clean_ver.to_string());
    }

    None
}

pub async fn refresh_codebuddy_cli_version(
    upstream_client: &Arc<UpstreamClient>,
) -> Option<String> {
    refresh_codebuddy_version(upstream_client).await
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CodeBuddyAdapter {
    config: ProviderAdapterConfig,
    /// Snapshot del contrato congelado por petición (ver
    /// [`ProviderAdapter::pin_request_contract`]): los headers de la petición
    /// leen la MISMA generación aunque el runtime global refresque en
    /// paralelo. No se persiste: es estado de ejecución, no configuración.
    #[serde(skip, default)]
    pinned: Option<Arc<Snapshot<CodeBuddyProfile>>>,
}

impl CodeBuddyAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("codebuddy"),
                name: "CodeBuddy".into(),
                anonymous_fallback: false,
                rate_limit_scope: "account".into(),
                base_url: codebuddy_base_url(),
                auth_type: AdapterAuthType::OAuth,
                format: AdapterFormat::Openai,
                extra_headers: vec![],
            },
            pinned: None,
        }
    }

    /// El snapshot del contrato congelado para esta petición, si hay uno.
    #[must_use]
    pub fn pinned_contract(&self) -> Option<&Arc<Snapshot<CodeBuddyProfile>>> {
        self.pinned.as_ref()
    }

    /// Congela un snapshot explícito: los headers de la petición leen esa
    /// generación concreta. Lo usa el pipeline cuando ya tiene el snapshot
    /// memoizado, y los tests.
    #[must_use]
    pub fn with_pinned_contract(mut self, pinned: Arc<Snapshot<CodeBuddyProfile>>) -> Self {
        self.pinned = Some(pinned);
        self
    }
}

crate::adapters::derive_default_from_new!(CodeBuddyAdapter);

impl ProviderAdapter for CodeBuddyAdapter {
    crate::adapters::impl_adapter_config!();

    fn metadata(&self) -> openproxy_types::ProviderMetadata {
        let mut meta = openproxy_types::ProviderMetadata::custom_default();
        meta.built_in = true;
        meta.deletable = false;
        meta.supports_quota = true;
        meta.quota_refresh_supported = true;
        meta.requires_oauth = true;
        meta.oauth_refresh_lead_seconds = Some(300);
        meta
    }

    fn models_url(&self) -> Option<String> {
        None
    }

    fn build_auth_header(&self, access_token: &str) -> Option<(String, String)> {
        let trimmed = access_token.trim();
        if trimmed.is_empty() {
            return None;
        }
        let val = if trimmed.starts_with("Bearer ") {
            trimmed.to_string()
        } else {
            format!("Bearer {trimmed}")
        };
        Some(("Authorization".into(), val))
    }

    fn pin_request_contract(&mut self) {
        // Congela el snapshot activo (LKG en disco o semilla embebida): los
        // headers de la petición leen la MISMA generación aunque el runtime
        // global refresque en paralelo. Sin runtime disponible el adapter
        // sigue funcionando con su identidad configurada: el contrato es
        // opt-in por petición.
        self.pinned = codebuddy_state::ensure_loaded().ok();
    }

    fn build_headers(
        &self,
        access_token: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let mut headers = super::build_spoofer_headers(
            self.build_auth_header(access_token),
            &CodeBuddySpoofer,
            &[],
        );
        // el refresher o la metadata de npm (fallback) es visible de inmediato.
        let mut cur_ver = current_codebuddy_version();
        let mut cur_ua = current_codebuddy_ua();
        if let Some(snapshot) = &self.pinned
            && snapshot.payload.identity_activatable
            && let Some(version) = snapshot.client_identity.version.as_deref()
        {
            cur_ver = version.to_string();
            cur_ua = format!("CLI/{version} CodeBuddy/{version}");
        }
        crate::spoofer::upsert_header(&mut headers, "x-ide-version", cur_ver);
        crate::spoofer::upsert_header(&mut headers, "user-agent", cur_ua);

        // Required conversation and message tracing headers observed in official CLI
        // to pass Tencent backend channel verification (error 11128).
        let conv_id = uuid::Uuid::new_v4().to_string();
        let msg_id = uuid::Uuid::new_v4().simple().to_string();
        let req_id = uuid::Uuid::new_v4().to_string();
        crate::spoofer::upsert_header(&mut headers, "x-conversation-id", conv_id);
        crate::spoofer::upsert_header(&mut headers, "x-conversation-request-id", req_id);
        crate::spoofer::upsert_header(&mut headers, "x-conversation-message-id", msg_id);

        // El override de configuración gana sobre el contrato y sobre el
        // spoofer: es la decisión explícita del despliegue.
        crate::spoofer::merge_header_refs(&mut headers, &self.config.extra_headers);
        headers
    }

    fn build_chat_url(&self, target_format: TargetFormat, model: &ModelId) -> String {
        let _ = (target_format, model);
        format!("{}/chat/completions", self.config.base_url)
    }

    fn fetch_models(
        &self,
        _upstream_client: &Arc<UpstreamClient>,
        _api_key: &str,
    ) -> impl std::future::Future<Output = Result<Vec<DiscoveredModel>>> + Send {
        // Discovery consume el catálogo del snapshot sin elevar la identidad
        // ni iniciar otra ronda de refresh. El catálogo estático es fallback.
        let snapshot = self
            .pinned
            .clone()
            .or_else(|| codebuddy_state::ensure_loaded().ok());
        let models = snapshot
            .as_deref()
            .and_then(models::codebuddy_contract_models)
            .unwrap_or_else(codebuddy_static_models);
        std::future::ready(Ok(models))
    }

    fn models_dev_canonical_ids(&self) -> &'static [&'static str] {
        &["codebuddy", "tencent"]
    }

    async fn fetch_quota(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
        access_token: Option<&str>,
        provider_specific: Option<&str>,
    ) -> Option<Result<openproxy_types::AccountQuota>> {
        self.fetch_quota_with_proxy(
            upstream_client,
            api_key,
            access_token,
            provider_specific,
            None,
        )
        .await
    }

    async fn fetch_quota_with_proxy(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
        access_token: Option<&str>,
        provider_specific: Option<&str>,
        proxy_url: Option<&str>,
    ) -> Option<Result<openproxy_types::AccountQuota>> {
        let token = access_token.unwrap_or(api_key).trim();
        if token.is_empty() {
            return Some(Ok(openproxy_types::AccountQuota::with_error(
                "codebuddy requires OAuth access token for quota",
            )));
        }

        Some(
            fetch_codebuddy_quota_unified(upstream_client, token, provider_specific, proxy_url)
                .await,
        )
    }
    fn normalize_openai_request(&self, view: &mut openproxy_types::OpenAIRequestView) {
        // Upstream rejects non-streaming chat with error 11101. Streaming lets the
        // unary dispatcher accumulate the SSE back into a unary OpenAIResponse.
        view.stream = true;

        // CodeBuddy upstream security policy requires first message to be role: "system" (error 11128).
        ensure_codebuddy_system_prompt_in_view(view);
    }

    fn wrap_request_body(
        &self,
        body: bytes::Bytes,
        _target_format: TargetFormat,
        _model: &ModelId,
        _resolved_target: &openproxy_types::context::ResolvedTarget,
    ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
        crate::adapters::traits::patch_json_request_body(body, patch_codebuddy_request_object)
    }
}

fn patch_codebuddy_request_object(obj: &mut serde_json::Map<String, serde_json::Value>) {
    if let Some(serde_json::Value::String(model_str)) = obj.get_mut("model") {
        normalize_codebuddy_model_name(model_str);
    }

    if obj.get("stream") != Some(&serde_json::Value::Bool(true)) {
        obj.insert("stream".to_string(), serde_json::Value::Bool(true));
    }

    if let Some(messages) = obj.get_mut("messages").and_then(|m| m.as_array_mut())
        && (messages.is_empty()
            || messages[0].get("role").and_then(|r| r.as_str()) != Some("system")
            || messages
                .iter()
                .any(|m| m.get("role").and_then(|r| r.as_str()) == Some("developer")))
    {
        ensure_codebuddy_system_prompt_json(messages);
    }
}

pub const DEFAULT_CODEBUDDY_SYSTEM_PROMPT: &str =
    "You are CodeBuddy, a helpful AI coding assistant.";

/// Prepends `DEFAULT_CODEBUDDY_SYSTEM_PROMPT` unless message 0 is already
/// system/developer, in which case an empty one is filled in.
///
/// Later messages are never plucked or reordered: the prefix token sequence
/// has to stay identical across turns for upstream automatic KV prompt
/// caching.
pub fn ensure_codebuddy_system_prompt_in_view(view: &mut openproxy_types::OpenAIRequestView) {
    let messages = view.messages.to_mut();
    if messages.is_empty() {
        messages.push(openproxy_types::OpenAIMessage {
            role: "system".to_string(),
            content: Some(serde_json::Value::String(
                DEFAULT_CODEBUDDY_SYSTEM_PROMPT.to_string(),
            )),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::default(),
        });
        return;
    }

    if messages[0].role == "system" || messages[0].role == "developer" {
        if messages[0].role == "developer" {
            messages[0].role = "system".to_string();
        }
        let is_empty = match &messages[0].content {
            None => true,
            Some(serde_json::Value::Null) => true,
            Some(serde_json::Value::String(s)) => s.trim().is_empty(),
            Some(serde_json::Value::Array(a)) => a.is_empty(),
            _ => false,
        };
        if is_empty {
            messages[0].content = Some(serde_json::Value::String(
                DEFAULT_CODEBUDDY_SYSTEM_PROMPT.to_string(),
            ));
        }
        for m in messages.iter_mut().skip(1) {
            if m.role == "developer" {
                m.role = "system".to_string();
            }
        }
        return;
    }

    messages.insert(
        0,
        openproxy_types::OpenAIMessage {
            role: "system".to_string(),
            content: Some(serde_json::Value::String(
                DEFAULT_CODEBUDDY_SYSTEM_PROMPT.to_string(),
            )),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::default(),
        },
    );

    for m in messages.iter_mut().skip(1) {
        if m.role == "developer" {
            m.role = "system".to_string();
        }
    }
}

pub fn ensure_codebuddy_system_prompt_json(messages: &mut Vec<serde_json::Value>) {
    if messages.is_empty() {
        messages.push(serde_json::json!({
            "role": "system",
            "content": DEFAULT_CODEBUDDY_SYSTEM_PROMPT,
        }));
        return;
    }

    let first_role = messages[0]
        .get("role")
        .and_then(|r| r.as_str())
        .unwrap_or("");
    if first_role == "system" || first_role == "developer" {
        if first_role == "developer"
            && let Some(obj) = messages[0].as_object_mut()
        {
            obj.insert(
                "role".to_string(),
                serde_json::Value::String("system".to_string()),
            );
        }
        let content_empty = messages[0].get("content").is_none_or(|c| {
            c.is_null()
                || (c.is_string() && c.as_str().unwrap_or("").trim().is_empty())
                || (c.is_array() && c.as_array().is_some_and(|a| a.is_empty()))
        });
        if content_empty && let Some(obj) = messages[0].as_object_mut() {
            obj.insert(
                "content".to_string(),
                serde_json::Value::String(DEFAULT_CODEBUDDY_SYSTEM_PROMPT.to_string()),
            );
        }
        for m in messages.iter_mut().skip(1) {
            if m.get("role").and_then(|r| r.as_str()) == Some("developer")
                && let Some(obj) = m.as_object_mut()
            {
                obj.insert(
                    "role".to_string(),
                    serde_json::Value::String("system".to_string()),
                );
            }
        }
        return;
    }

    messages.insert(
        0,
        serde_json::json!({
            "role": "system",
            "content": DEFAULT_CODEBUDDY_SYSTEM_PROMPT,
        }),
    );

    for m in messages.iter_mut().skip(1) {
        if m.get("role").and_then(|r| r.as_str()) == Some("developer")
            && let Some(obj) = m.as_object_mut()
        {
            obj.insert(
                "role".to_string(),
                serde_json::Value::String("system".to_string()),
            );
        }
    }
}

/// Known business error codes from CodeBuddy / Tencent Cloud Copilot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeBuddyErrorCode {
    // Protocol constraints
    NonStreamNotSupported = 11101,
    FirstMessageNotSystemPrompt = 11128,
    // 6000..=6008 rate limits
    CraftRateLimit = 6000,
    CraftRateTPSLimit = 6001,
    CraftRateTPMLimit = 6002,
    CraftRateTPHLimit = 6003,
    CraftRateTPDLimit = 6004,
    CraftRateRPSLimit = 6005,
    CraftRateRPMLimit = 6006,
    CraftRateRPHLimit = 6007,
    CraftRateRPDLimit = 6008,

    // Quota & credits
    UsageLimitExceeded = 14001,
    ConversationChatTooMany = 14002,
    RateLimitError = 14003,
    UsageLimitExceededEnterprise = 14012,
    UsageLimitExceededTencent = 14013,
    UsageLimitEnterpriseExhausted = 14014,
    UsageLimitLicenseExpired = 14015,
    UsageLimitEnterpriseNotActivated = 14016,
    UsageLimitUserNotActivated = 14017,
    UsageLimitUserExhausted = 14018,
    UsageLimitNoTokenBudget = 14019,

    // Session & web search & context
    ConversationLimitExceeded = 10105,
    WebSearchRateLimit = 15001,
    ContextTooLong = 11115,

    // Auth
    AuthForbidden1 = 11140,
    ModelBehaviorError = 11141,
    AuthForbidden2 = 11142,
}

impl CodeBuddyErrorCode {
    pub fn from_code(code: u32) -> Option<Self> {
        match code {
            6000 => Some(Self::CraftRateLimit),
            6001 => Some(Self::CraftRateTPSLimit),
            6002 => Some(Self::CraftRateTPMLimit),
            6003 => Some(Self::CraftRateTPHLimit),
            6004 => Some(Self::CraftRateTPDLimit),
            6005 => Some(Self::CraftRateRPSLimit),
            6006 => Some(Self::CraftRateRPMLimit),
            6007 => Some(Self::CraftRateRPHLimit),
            6008 => Some(Self::CraftRateRPDLimit),
            14001 => Some(Self::UsageLimitExceeded),
            14002 => Some(Self::ConversationChatTooMany),
            14003 => Some(Self::RateLimitError),
            14012 => Some(Self::UsageLimitExceededEnterprise),
            14013 => Some(Self::UsageLimitExceededTencent),
            14014 => Some(Self::UsageLimitEnterpriseExhausted),
            14015 => Some(Self::UsageLimitLicenseExpired),
            14016 => Some(Self::UsageLimitEnterpriseNotActivated),
            14017 => Some(Self::UsageLimitUserNotActivated),
            14018 => Some(Self::UsageLimitUserExhausted),
            14019 => Some(Self::UsageLimitNoTokenBudget),
            10105 => Some(Self::ConversationLimitExceeded),
            15001 => Some(Self::WebSearchRateLimit),
            11101 => Some(Self::NonStreamNotSupported),
            11128 => Some(Self::FirstMessageNotSystemPrompt),
            11115 => Some(Self::ContextTooLong),
            11140 => Some(Self::AuthForbidden1),
            11141 => Some(Self::ModelBehaviorError),
            11142 => Some(Self::AuthForbidden2),
            _ => None,
        }
    }

    pub fn is_rate_limit(self) -> bool {
        matches!(
            self,
            Self::CraftRateLimit
                | Self::CraftRateTPSLimit
                | Self::CraftRateTPMLimit
                | Self::CraftRateTPHLimit
                | Self::CraftRateTPDLimit
                | Self::CraftRateRPSLimit
                | Self::CraftRateRPMLimit
                | Self::CraftRateRPHLimit
                | Self::CraftRateRPDLimit
                | Self::RateLimitError
                | Self::WebSearchRateLimit
        )
    }

    pub fn is_credits_exhausted(self) -> bool {
        matches!(
            self,
            Self::UsageLimitExceeded
                | Self::ConversationChatTooMany
                | Self::UsageLimitExceededEnterprise
                | Self::UsageLimitExceededTencent
                | Self::UsageLimitEnterpriseExhausted
                | Self::UsageLimitUserExhausted
                | Self::UsageLimitNoTokenBudget
        )
    }

    pub fn is_auth_error(self) -> bool {
        matches!(
            self,
            Self::UsageLimitLicenseExpired
                | Self::UsageLimitEnterpriseNotActivated
                | Self::UsageLimitUserNotActivated
                | Self::AuthForbidden1
                | Self::AuthForbidden2
        )
    }

    pub fn error_subcategory(self) -> &'static str {
        match self {
            Self::CraftRateLimit
            | Self::CraftRateTPSLimit
            | Self::CraftRateTPMLimit
            | Self::CraftRateTPHLimit
            | Self::CraftRateTPDLimit => "quota_token_limit",
            Self::CraftRateRPSLimit
            | Self::CraftRateRPMLimit
            | Self::CraftRateRPHLimit
            | Self::CraftRateRPDLimit
            | Self::RateLimitError => "quota_request_limit",
            Self::UsageLimitExceeded
            | Self::ConversationChatTooMany
            | Self::UsageLimitExceededEnterprise
            | Self::UsageLimitExceededTencent
            | Self::UsageLimitEnterpriseExhausted
            | Self::UsageLimitUserExhausted
            | Self::UsageLimitNoTokenBudget => "quota_balance_exhausted",
            Self::UsageLimitLicenseExpired => "auth_expired",
            Self::UsageLimitEnterpriseNotActivated | Self::UsageLimitUserNotActivated => {
                "quota_not_activated"
            }
            Self::ConversationLimitExceeded => "quota_active_session",
            Self::WebSearchRateLimit => "quota_web_search",
            Self::NonStreamNotSupported => "non_stream_not_supported",
            Self::FirstMessageNotSystemPrompt => "first_message_not_system_prompt",
            Self::ContextTooLong => "model_input_too_long",
            Self::AuthForbidden1 | Self::AuthForbidden2 => "auth_forbidden",
            Self::ModelBehaviorError => "model_behavior_error",
        }
    }

    pub fn to_upstream_error_class(self) -> openproxy_types::UpstreamErrorClass {
        if self.is_rate_limit() || self.is_credits_exhausted() {
            openproxy_types::UpstreamErrorClass::ResourceExhausted
        } else if self.is_auth_error() {
            openproxy_types::UpstreamErrorClass::PermissionDenied
        } else if self == Self::ContextTooLong || self == Self::NonStreamNotSupported {
            openproxy_types::UpstreamErrorClass::InvalidPayload
        } else if self == Self::FirstMessageNotSystemPrompt {
            // Error 11128: histéricamente "first message is not system prompt", pero Tencent
            // también lo usa como "request illegal" por channel verification ("This call failed
            // channel verification, possibly because it was not sent from an official client").
            // Clasificado como ValidationRequired para que el combo descarte la cuenta/target
            // actual y rote de inmediato a la siguiente en lugar de fallar la petición completa
            // al cliente como error de payload.
            openproxy_types::UpstreamErrorClass::ValidationRequired
        } else {
            openproxy_types::UpstreamErrorClass::Generic
        }
    }
}

fn extract_biz_code(val: &serde_json::Value, max_depth: usize) -> Option<u32> {
    if max_depth == 0 {
        return None;
    }
    // >= 1000 excludes HTTP status codes and JSON-RPC codes.
    for key in ["code", "errcode"] {
        if let Some(num) = val.get(key).and_then(serde_json::Value::as_i64)
            && (1_000..=(u32::MAX as i64)).contains(&num)
        {
            return Some(num as u32);
        }
        if let Some(s) = val.get(key).and_then(serde_json::Value::as_str)
            && let Ok(num) = s.parse::<u32>()
            && num >= 1_000
        {
            return Some(num);
        }
    }
    // Descend into nested JSON-RPC shells and Axios error envelopes.
    for key in ["data", "error", "response"] {
        if let Some(child) = val.get(key)
            && let Some(code) = extract_biz_code(child, max_depth - 1)
        {
            return Some(code);
        }
    }
    None
}

pub fn parse_codebuddy_error_code(body: &str) -> Option<u32> {
    let val: serde_json::Value = serde_json::from_str(body).ok()?;
    extract_biz_code(&val, 6)
}
