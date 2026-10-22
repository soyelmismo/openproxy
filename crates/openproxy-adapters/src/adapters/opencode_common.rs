//! Shared OpenCode logic (Zen & Go variants).
//!
//! OpenCode Zen and OpenCode Go share identical model classification heuristics,
//! request headers (including client spoofing and per-format auth branching),
//! and model list discovery format.

use super::{
    Arc, CoreError, DiscoveredModel, OpenAIModelsResponse, ProviderAdapter, Result, TargetFormat,
    UpstreamClient, upstream_get_json,
};
use crate::spoofer::{ClientSpoofer, OpenCodeSpoofer};
use openproxy_types::{AccountQuota, ModelQuotaDetail, ProviderMetadata, ResultExt};

// Los endpoints integrados son autoridad del código, no de la fila persistida.
const OPENCODE_ZEN_BASE_URL: &str = "https://opencode.ai/inference/v1";
const OPENCODE_GO_BASE_URL: &str = "https://opencode.ai/inference/go/v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OpenCodeFlavor {
    Zen,
    Go,
}

pub fn classify_zen_target_format(id: &str) -> TargetFormat {
    classify_opencode_target_format(OpenCodeFlavor::Zen, id)
}

pub fn classify_go_target_format(id: &str) -> TargetFormat {
    classify_opencode_target_format(OpenCodeFlavor::Go, id)
}

/// Wire target format per the OpenCode Console routing tables:
/// - Anthropic `/messages`: claude, minimax, qwen, union-alpha
/// - Gemini `/models/{model}:streamGenerateContent?alt=sse`: gemini
/// - Responses `/responses`: gpt-5, gpt-6, grok, muse-spark
/// - OpenAI `/chat/completions`: deepseek, glm, kimi, mimo, ling, nemotron,
///   big-pickle, etc.
///
/// Exact-ID overrides run before the family heuristic because Zen and Go expose
/// different backends under the same alias, and the catalogue mixes wire
/// formats inside a family: Zen answers `minimax-m3` on `/chat/completions` but
/// `minimax-m3-free` only on `/messages`.
///
/// Sources: the endpoints tables at <https://opencode.ai/docs/zen> and
/// <https://opencode.ai/docs/go>, plus the per-model `provider.npm` metadata of
/// the `opencode` / `opencode-go` entries in models.dev.
pub fn classify_opencode_target_format(flavor: OpenCodeFlavor, id: &str) -> TargetFormat {
    let lower = id.to_ascii_lowercase();
    match (flavor, lower.as_str()) {
        // `messages`-only, with no Anthropic substring to match on.
        (_, "union-alpha") => TargetFormat::Anthropic,
        (
            OpenCodeFlavor::Zen,
            "minimax-m2.1" | "minimax-m2.5" | "minimax-m2.7" | "minimax-m3" | "qwen3-coder"
            | "grok-code",
        ) => TargetFormat::Openai,
        _ => family_target_format(&lower),
    }
}

fn family_target_format(lower: &str) -> TargetFormat {
    if lower.contains("jev") || lower.contains("systemone") {
        TargetFormat::SystemOne
    } else if lower.contains("claude") || lower.contains("minimax") || lower.contains("qwen") {
        TargetFormat::Anthropic
    } else if lower.contains("gemini") {
        TargetFormat::Gemini
    } else if lower.contains("gpt-5")
        || lower.contains("gpt-6")
        || lower.contains("grok")
        || lower.contains("muse-spark")
    {
        TargetFormat::Responses
    } else {
        TargetFormat::Openai
    }
}

fn append_format_auth_headers(
    headers: &mut Vec<(String, String)>,
    adapter: &impl ProviderAdapter,
    api_key: &str,
    target_format: TargetFormat,
) {
    let effective_key = if api_key.is_empty() {
        "public"
    } else {
        api_key
    };
    if target_format == TargetFormat::Anthropic {
        headers.push(("x-api-key".into(), effective_key.to_string()));
        headers.push(("Anthropic-Version".into(), "2023-06-01".into()));
    } else if target_format == TargetFormat::Gemini {
        headers.push(("x-goog-api-key".into(), effective_key.to_string()));
    } else if let Some(auth) = adapter.build_auth_header(effective_key) {
        headers.push(auth);
    } else {
        headers.push(crate::upstream::bearer_auth_header(effective_key));
    }
}

pub fn build_opencode_headers(
    adapter: &impl ProviderAdapter,
    api_key: &str,
    target_format: TargetFormat,
) -> Vec<(String, String)> {
    let mut headers = vec![("Content-Type".into(), "application/json".into())];

    append_format_auth_headers(&mut headers, adapter, api_key, target_format);

    headers.extend(OpenCodeSpoofer.headers());

    for (k, v) in &adapter.config().extra_headers {
        // La atribución de un cliente OpenRouter no pertenece al contrato
        // OpenCode y provoca FreeTierError incluso con autenticación válida.
        if k.eq_ignore_ascii_case("HTTP-Referer") {
            continue;
        }
        // Un override vacío no puede borrar la credencial resuelta ni el
        // marcador público del acceso anónimo. Los no vacíos siguen vigentes.
        if v.trim().is_empty()
            && ["Authorization", "x-api-key", "x-goog-api-key"]
                .iter()
                .any(|name| k.eq_ignore_ascii_case(name))
        {
            continue;
        }
        if let Some(pos) = headers
            .iter()
            .position(|(hk, _)| hk.eq_ignore_ascii_case(k))
        {
            headers[pos].1 = v.clone();
        } else {
            headers.push((k.clone(), v.clone()));
        }
    }

    headers
}

pub async fn fetch_opencode_models(
    adapter: &impl ProviderAdapter,
    flavor: OpenCodeFlavor,
    upstream_client: &Arc<UpstreamClient>,
    api_key: &str,
) -> Result<Vec<DiscoveredModel>> {
    // 0. Sync CLI version from npm registry into memory
    let _ = crate::spoofer::refresh_opencode_version(upstream_client).await;

    let url = adapter
        .models_url()
        .ok_or_else(|| CoreError::Validation(format!("{}: models_url is None", adapter.id())))?;

    let effective_key = if api_key.is_empty() {
        "public"
    } else {
        api_key
    };
    let auth = crate::upstream::format_bearer(effective_key);
    let body = upstream_get_json(
        upstream_client,
        &url,
        &[
            ("Authorization", &auth),
            ("User-Agent", crate::spoofer::OPENCODE_UA),
        ],
    )
    .await
    .ctx_upstream(format!("{} /models", adapter.id()))?;

    let payload: OpenAIModelsResponse =
        <OpenAIModelsResponse as serde::Deserialize>::deserialize(&body)
            .ctx_validation(format!("{} /models parse", adapter.id()))?;

    let out = payload
        .data
        .into_iter()
        .map(|m| {
            let target_format = classify_opencode_target_format(flavor, &m.id);
            super::build_discovered_model_with(m.id, target_format)
        })
        .collect();
    Ok(out)
}

pub const OPENCODE_GO_USAGE_URL: &str = "https://opencode.ai/inference/go/v1/usage";

pub async fn fetch_opencode_go_quota(
    upstream_client: &Arc<UpstreamClient>,
    api_key: &str,
    proxy_url: Option<&str>,
) -> Result<AccountQuota> {
    let mut req = crate::upstream::UpstreamRequest::get_json(OPENCODE_GO_USAGE_URL)
        .with_bearer(api_key);
    req.headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static(crate::spoofer::OPENCODE_UA),
    );
    if let Some(proxy) = proxy_url {
        req.proxy = Some(proxy.to_string());
    }

    let cancel = crate::upstream::CancellationToken::new();
    let resp = upstream_client
        .call(req, crate::upstream::TimeoutProfile::Quota, cancel)
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("opencode-go quota: {e}")))?;

    let status = resp.status;
    let body = resp
        .collect()
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("opencode-go quota body: {e}")))?;

    if !status.is_success() {
        let body_str = String::from_utf8_lossy(&body);
        return Err(CoreError::upstream_error(
            status.as_u16(),
            "opencode-go",
            "<quota>",
            body_str.to_string(),
            false,
        ));
    }

    let json: serde_json::Value =
        serde_json::from_slice(&body).ctx_parse("opencode-go quota json parse error")?;

    Ok(parse_opencode_go_usage_response(&json))
}

pub(crate) fn parse_opencode_go_usage_response(json: &serde_json::Value) -> AccountQuota {
    let usage = json.get("usage").and_then(|v| v.as_object());

    let (session_used, session_limit, session_reset_at) = usage
        .and_then(|u| u.get("rolling"))
        .and_then(|v| v.as_object())
        .map_or((None, None, None), |r| {
            let pct = r.get("percent").and_then(|p| p.as_i64()).unwrap_or(0);
            let resets_at = r.get("resetsAt").and_then(|s| s.as_str()).map(ToString::to_string);
            (Some(pct), Some(100), resets_at)
        });

    let (weekly_used, weekly_limit, weekly_reset_at) = usage
        .and_then(|u| u.get("weekly"))
        .and_then(|v| v.as_object())
        .map_or((None, None, None), |w| {
            let pct = w.get("percent").and_then(|p| p.as_i64()).unwrap_or(0);
            let resets_at = w.get("resetsAt").and_then(|s| s.as_str()).map(ToString::to_string);
            (Some(pct), Some(100), resets_at)
        });

    let monthly_detail = usage
        .and_then(|u| u.get("monthly"))
        .and_then(|v| v.as_object())
        .map(|m| {
            let pct = m.get("percent").and_then(|p| p.as_i64()).unwrap_or(0);
            let resets_at = m.get("resetsAt").and_then(|s| s.as_str()).map(ToString::to_string);
            let remaining_fraction = ((100 - pct).max(0) as f64) / 100.0;
            ModelQuotaDetail {
                model_id: "Monthly".to_string(),
                session_used: pct,
                session_limit: 100,
                session_reset_at: resets_at,
                remaining_fraction,
            }
        });

    let model_details = monthly_detail.map(|d| vec![d].into_boxed_slice());

    AccountQuota {
        session_used,
        session_limit,
        session_reset_at,
        weekly_used,
        weekly_limit,
        weekly_reset_at,
        plan_name: Some("OpenCode Go".to_string()),
        last_fetched_at: openproxy_types::now_unix_secs_str(),
        fetch_error: None,
        model_details,
        pools: None,
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct OpenCodeAdapter {
    flavor: OpenCodeFlavor,
    config: crate::adapters::ProviderAdapterConfig,
}

impl OpenCodeAdapter {
    pub fn new(flavor: OpenCodeFlavor) -> Self {
        let (id, name, base_url) = match flavor {
            OpenCodeFlavor::Zen => ("opencode-zen", "OpenCode Zen", OPENCODE_ZEN_BASE_URL),
            OpenCodeFlavor::Go => ("opencode-go", "OpenCode Go", OPENCODE_GO_BASE_URL),
        };

        Self {
            flavor,
            config: crate::adapters::ProviderAdapterConfig {
                id: openproxy_types::ProviderId::new(id),
                name: name.into(),
                anonymous_fallback: true,
                rate_limit_scope: "account".into(),
                base_url: base_url.into(),
                auth_type: crate::adapters::AdapterAuthType::Bearer,
                format: crate::adapters::AdapterFormat::Mixed,
                extra_headers: vec![],
            },
        }
    }
    pub fn wrap_request_body(
        &self,
        body: bytes::Bytes,
        target_format: TargetFormat,
        model: &openproxy_types::ModelId,
        resolved_target: &openproxy_types::context::ResolvedTarget,
    ) -> Result<bytes::Bytes> {
        let api_key = resolved_target
            .custom_meta
            .as_ref()
            .map_or(resolved_target.api_key.as_str(), |m| {
                m.access_token.as_str()
            });

        if !is_free_opencode_tier(self.flavor, api_key, model) {
            return Ok(body);
        }

        if target_format == TargetFormat::SystemOne {
            if let Ok(mut val) = serde_json::from_slice::<serde_json::Value>(&body)
                && let Some(obj) = val.as_object_mut()
                && let Some(m_val) = obj.get_mut("model")
                && let Some(m_str) = m_val.as_str()
                && m_str.eq_ignore_ascii_case("jev-1.13")
            {
                *m_val = serde_json::Value::String("jev-1.13-free".to_string());
                if let Ok(re_encoded) = serde_json::to_vec(&val) {
                    return Ok(bytes::Bytes::from(re_encoded));
                }
            }
            return Ok(body);
        }

        crate::adapters::traits::patch_json_request_body(body, |obj| {
            if target_format != TargetFormat::Gemini {
                obj.insert("stream".to_string(), serde_json::Value::Bool(true));
            }
            if let Some(m_val) = obj.get_mut("model")
                && let Some(m_str) = m_val.as_str()
            {
                let m_lower = m_str.to_ascii_lowercase();
                if m_lower == "muse-spark-1.3" || m_lower == "muse-spark-1.3-contributor" {
                    *m_val =
                        serde_json::Value::String("muse-spark-1.3-contributor-free".to_string());
                } else if m_lower == "muse-spark-1.2" || m_lower == "muse-spark-1.2-contributor" {
                    *m_val =
                        serde_json::Value::String("muse-spark-1.2-contributor-free".to_string());
                } else if m_lower == "mimo-v2.5" {
                    *m_val = serde_json::Value::String("mimo-v2.5-free".to_string());
                } else if m_lower == "deepseek-v4-flash" {
                    *m_val = serde_json::Value::String("deepseek-v4-flash-free".to_string());
                } else if m_lower == "nemotron-3-ultra" {
                    *m_val = serde_json::Value::String("nemotron-3-ultra-free".to_string());
                } else if m_lower == "nemotron-3.5-lightning" {
                    *m_val = serde_json::Value::String("nemotron-3.5-lightning-free".to_string());
                } else if m_lower == "ling-3.0-flash-fin" {
                    *m_val = serde_json::Value::String("ling-3.0-flash-fin-free".to_string());
                }
            }
            inject_opencode_agent_quartet_tools(obj, target_format);
        })
    }

    pub fn config_mut(&mut self) -> Option<&mut crate::adapters::ProviderAdapterConfig> {
        Some(&mut self.config)
    }
}

/// Free tier on either Zen or Go requires a Bearer auth of `public` (or none) and
/// a mandatory agent quartet: `bash`, `glob`, `grep`, `read`, plus
/// `stream: true`.
pub fn is_free_opencode_tier(
    _flavor: OpenCodeFlavor,
    api_key: &str,
    model_id: &openproxy_types::ModelId,
) -> bool {
    let trimmed = api_key.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("public") {
        return true;
    }
    let m = model_id.as_str().to_ascii_lowercase();
    m.ends_with("-free")
        || m == "big-pickle"
        || m == "union-alpha"
        || m == "grok-code"
        || m.contains("contributor-free")
}

pub fn inject_opencode_agent_quartet_tools(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    target_format: TargetFormat,
) {
    let mut tools_arr = match obj.remove("tools") {
        Some(serde_json::Value::Array(arr)) => arr,
        _ => Vec::new(),
    };

    let mut existing_names = std::collections::HashSet::new();
    for t in &tools_arr {
        if let Some(name) = t
            .get("function")
            .and_then(|f| f.get("name"))
            .or_else(|| t.get("name"))
            .and_then(|n| n.as_str())
        {
            existing_names.insert(name.to_ascii_lowercase());
        } else if let Some(decls) = t.get("functionDeclarations").and_then(|d| d.as_array()) {
            for decl in decls {
                if let Some(name) = decl.get("name").and_then(|n| n.as_str()) {
                    existing_names.insert(name.to_ascii_lowercase());
                }
            }
        }
    }

    struct QuartetParam {
        name: &'static str,
        r#type: &'static str,
        desc: &'static str,
    }

    struct QuartetTool {
        name: &'static str,
        desc: &'static str,
        params: &'static [QuartetParam],
        required: &'static [&'static str],
    }

    const QUARTET: &[QuartetTool] = &[
        QuartetTool {
            name: "bash",
            desc: "Execute a shell command in the active workspace",
            params: &[
                QuartetParam {
                    name: "command",
                    r#type: "string",
                    desc: "Shell command string to execute",
                },
                QuartetParam {
                    name: "workdir",
                    r#type: "string",
                    desc: "Working directory",
                },
                QuartetParam {
                    name: "timeout",
                    r#type: "integer",
                    desc: "Timeout in milliseconds",
                },
            ],
            required: &["command"],
        },
        QuartetTool {
            name: "glob",
            desc: "Find files matching a glob pattern",
            params: &[
                QuartetParam {
                    name: "pattern",
                    r#type: "string",
                    desc: "Glob pattern to match files against",
                },
                QuartetParam {
                    name: "path",
                    r#type: "string",
                    desc: "Relative directory to search",
                },
                QuartetParam {
                    name: "limit",
                    r#type: "integer",
                    desc: "Maximum results to return",
                },
            ],
            required: &["pattern"],
        },
        QuartetTool {
            name: "grep",
            desc: "Search for regex matches in file contents",
            params: &[
                QuartetParam {
                    name: "pattern",
                    r#type: "string",
                    desc: "Regex pattern to search for in file contents",
                },
                QuartetParam {
                    name: "path",
                    r#type: "string",
                    desc: "Relative directory to search",
                },
                QuartetParam {
                    name: "include",
                    r#type: "string",
                    desc: "File glob to include in the search",
                },
                QuartetParam {
                    name: "limit",
                    r#type: "integer",
                    desc: "Maximum matches to return",
                },
            ],
            required: &["pattern"],
        },
        QuartetTool {
            name: "read",
            desc: "Read a text file or directory",
            params: &[
                QuartetParam {
                    name: "path",
                    r#type: "string",
                    desc: "Path to file or directory",
                },
                QuartetParam {
                    name: "offset",
                    r#type: "integer",
                    desc: "1-based line offset to start reading from",
                },
                QuartetParam {
                    name: "limit",
                    r#type: "integer",
                    desc: "Maximum entries or lines to read",
                },
            ],
            required: &["path"],
        },
    ];

    if target_format == TargetFormat::Gemini {
        let mut gemini_decls = Vec::new();
        for tool in QUARTET {
            if existing_names.contains(tool.name) {
                continue;
            }
            let mut properties = serde_json::Map::new();
            for p in tool.params {
                properties.insert(
                    p.name.to_string(),
                    serde_json::json!({
                        "type": p.r#type,
                        "description": p.desc,
                    }),
                );
            }
            let parameters = serde_json::json!({
                "type": "object",
                "properties": properties,
                "required": tool.required,
            });
            gemini_decls.push(serde_json::json!({
                "name": tool.name,
                "description": tool.desc,
                "parameters": parameters,
            }));
        }

        if !gemini_decls.is_empty() {
            if let Some(first_tool) = tools_arr.iter_mut().find_map(|t| {
                t.as_object_mut()
                    .and_then(|obj| obj.get_mut("functionDeclarations"))
                    .and_then(|fd| fd.as_array_mut())
            }) {
                first_tool.extend(gemini_decls);
            } else {
                tools_arr.push(serde_json::json!({
                    "functionDeclarations": gemini_decls,
                }));
            }
        }
    } else {
        for tool in QUARTET {
            if existing_names.contains(tool.name) {
                continue;
            }

            let mut properties = serde_json::Map::new();
            for p in tool.params {
                properties.insert(
                    p.name.to_string(),
                    serde_json::json!({
                        "type": p.r#type,
                        "description": p.desc,
                    }),
                );
            }
            let parameters = serde_json::json!({
                "type": "object",
                "properties": properties,
                "required": tool.required,
            });

            let tool_val = match target_format {
                TargetFormat::Anthropic => serde_json::json!({
                    "name": tool.name,
                    "description": tool.desc,
                    "input_schema": parameters,
                }),
                TargetFormat::Responses => serde_json::json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.desc,
                    "parameters": parameters,
                    "strict": false,
                }),
                _ => serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.desc,
                        "parameters": parameters,
                    }
                }),
            };
            tools_arr.push(tool_val);
        }
    }

    if target_format == TargetFormat::Responses {
        for t in &mut tools_arr {
            if let Some(t_obj) = t.as_object_mut()
                && !t_obj.contains_key("strict")
            {
                t_obj.insert("strict".to_string(), serde_json::Value::Bool(false));
            }
        }
    }

    obj.insert("tools".to_string(), serde_json::Value::Array(tools_arr));
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct OpenCodeGoAdapter(pub OpenCodeAdapter);

impl OpenCodeGoAdapter {
    pub fn new() -> Self {
        Self(OpenCodeAdapter::new(OpenCodeFlavor::Go))
    }
}
crate::adapters::derive_default_from_new!(OpenCodeGoAdapter);

macro_rules! delegate_opencode_adapter {
    ($adapter:ident) => {
        impl crate::adapters::ProviderAdapter for $adapter {
            fn config(&self) -> &crate::adapters::ProviderAdapterConfig {
                self.0.config()
            }
            fn config_mut(&mut self) -> Option<&mut crate::adapters::ProviderAdapterConfig> {
                self.0.config_mut()
            }
            fn is_anonymous_fallback(&self) -> bool {
                self.0.is_anonymous_fallback()
            }
            fn models_dev_canonical_ids(&self) -> &'static [&'static str] {
                self.0.models_dev_canonical_ids()
            }
            fn build_headers(
                &self,
                api_key: &str,
                target_format: openproxy_types::TargetFormat,
                model: &openproxy_types::ModelId,
            ) -> Vec<(String, String)> {
                self.0.build_headers(api_key, target_format, model)
            }
            fn build_chat_url(
                &self,
                target_format: openproxy_types::TargetFormat,
                model: &openproxy_types::ModelId,
            ) -> String {
                self.0.build_chat_url(target_format, model)
            }
            async fn fetch_models(
                &self,
                upstream_client: &std::sync::Arc<crate::upstream::UpstreamClient>,
                api_key: &str,
            ) -> openproxy_types::Result<Vec<openproxy_types::DiscoveredModel>> {
                self.0.fetch_models(upstream_client, api_key).await
            }
            fn metadata(&self) -> openproxy_types::ProviderMetadata {
                self.0.metadata()
            }
            fn fetch_quota_with_proxy(
                &self,
                upstream_client: &std::sync::Arc<crate::upstream::UpstreamClient>,
                api_key: &str,
                access_token: Option<&str>,
                provider_specific: Option<&str>,
                proxy_url: Option<&str>,
            ) -> impl std::future::Future<Output = Option<openproxy_types::Result<openproxy_types::AccountQuota>>> + Send {
                self.0.fetch_quota_with_proxy(upstream_client, api_key, access_token, provider_specific, proxy_url)
            }
            fn wrap_request_body(
                &self,
                body: bytes::Bytes,
                target_format: openproxy_types::TargetFormat,
                model: &openproxy_types::ModelId,
                resolved_target: &openproxy_types::context::ResolvedTarget,
            ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
                self.0
                    .wrap_request_body(body, target_format, model, resolved_target)
            }
        }
    };
}

delegate_opencode_adapter!(OpenCodeGoAdapter);

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct OpenCodeZenAdapter(pub OpenCodeAdapter);

impl OpenCodeZenAdapter {
    pub fn new() -> Self {
        Self(OpenCodeAdapter::new(OpenCodeFlavor::Zen))
    }
}
crate::adapters::derive_default_from_new!(OpenCodeZenAdapter);

delegate_opencode_adapter!(OpenCodeZenAdapter);

impl crate::adapters::ProviderAdapter for OpenCodeAdapter {
    fn config(&self) -> &crate::adapters::ProviderAdapterConfig {
        &self.config
    }

    fn config_mut(&mut self) -> Option<&mut crate::adapters::ProviderAdapterConfig> {
        Some(&mut self.config)
    }

    fn is_anonymous_fallback(&self) -> bool {
        true
    }

    fn models_dev_canonical_ids(&self) -> &'static [&'static str] {
        match self.flavor {
            OpenCodeFlavor::Zen => &["opencode"],
            OpenCodeFlavor::Go => &["opencode-go"],
        }
    }

    fn build_headers(
        &self,
        api_key: &str,
        target_format: openproxy_types::TargetFormat,
        _model: &openproxy_types::ModelId,
    ) -> Vec<(String, String)> {
        build_opencode_headers(self, api_key, target_format)
    }

    fn build_chat_url(
        &self,
        target_format: openproxy_types::TargetFormat,
        model: &openproxy_types::ModelId,
    ) -> String {
        let base_url = &self.config.base_url;
        if target_format == openproxy_types::TargetFormat::Gemini {
            format!(
                "{base_url}/models/{}:streamGenerateContent?alt=sse",
                model.as_str()
            )
        } else {
            let eff_format =
                crate::adapters::traits::resolve_target_format(self.config.format, target_format);
            format!(
                "{base_url}{}",
                crate::adapters::traits::target_format_path(eff_format)
            )
        }
    }

    async fn fetch_models(
        &self,
        upstream_client: &std::sync::Arc<crate::upstream::UpstreamClient>,
        api_key: &str,
    ) -> openproxy_types::Result<Vec<openproxy_types::DiscoveredModel>> {
        fetch_opencode_models(self, self.flavor, upstream_client, api_key).await
    }

    fn metadata(&self) -> ProviderMetadata {
        let supports_quota = matches!(self.flavor, OpenCodeFlavor::Go);
        ProviderMetadata {
            built_in: true,
            deletable: false,
            supports_quota,
            quota_refresh_supported: supports_quota,
            requires_oauth: false,
            oauth_refresh_lead_seconds: None,
        }
    }

    async fn fetch_quota(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
        access_token: Option<&str>,
        _provider_specific: Option<&str>,
    ) -> Option<Result<AccountQuota>> {
        self.fetch_quota_with_proxy(upstream_client, api_key, access_token, _provider_specific, None).await
    }

    async fn fetch_quota_with_proxy(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
        access_token: Option<&str>,
        _provider_specific: Option<&str>,
        proxy_url: Option<&str>,
    ) -> Option<Result<AccountQuota>> {
        if self.flavor != OpenCodeFlavor::Go {
            return None;
        }

        let token = access_token.unwrap_or(api_key).trim();
        if token.is_empty() {
            return Some(Ok(AccountQuota::with_error(
                "opencode-go requires API key for quota",
            )));
        }

        Some(fetch_opencode_go_quota(upstream_client, token, proxy_url).await)
    }

    fn wrap_request_body(
        &self,
        body: bytes::Bytes,
        target_format: openproxy_types::TargetFormat,
        model: &openproxy_types::ModelId,
        resolved_target: &openproxy_types::context::ResolvedTarget,
    ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
        self.wrap_request_body(body, target_format, model, resolved_target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn test_parse_opencode_go_usage_response() {
        let json = serde_json::json!({
            "usage": {
                "rolling": { "status": "ok", "percent": 5, "resetsAt": "2026-10-23T03:04:10.000Z" },
                "weekly": { "status": "ok", "percent": 25, "resetsAt": "2026-10-26T00:00:00.000Z" },
                "monthly": { "status": "ok", "percent": 80, "resetsAt": "2026-11-07T00:25:10.000Z" }
            }
        });

        let quota = parse_opencode_go_usage_response(&json);
        assert_eq!(quota.session_used, Some(5));
        assert_eq!(quota.session_limit, Some(100));
        assert_eq!(quota.session_reset_at.as_deref(), Some("2026-10-23T03:04:10.000Z"));

        assert_eq!(quota.weekly_used, Some(25));
        assert_eq!(quota.weekly_limit, Some(100));
        assert_eq!(quota.weekly_reset_at.as_deref(), Some("2026-10-26T00:00:00.000Z"));

        assert_eq!(quota.plan_name.as_deref(), Some("OpenCode Go"));

        let details = quota.model_details.expect("model_details should be present");
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].model_id, "Monthly");
        assert_eq!(details[0].session_used, 80);
        assert_eq!(details[0].session_limit, 100);
        assert_eq!(details[0].session_reset_at.as_deref(), Some("2026-11-07T00:25:10.000Z"));
        assert!((details[0].remaining_fraction - 0.20).abs() < f64::EPSILON);
    }

    #[test]
    fn test_parse_opencode_go_usage_response_empty() {
        let json = serde_json::json!({});
        let quota = parse_opencode_go_usage_response(&json);
        assert!(quota.session_used.is_none());
        assert!(quota.weekly_used.is_none());
        assert!(quota.model_details.is_none());
    }

    #[test]
    fn test_inject_opencode_agent_quartet_tools_responses_strict_false() {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "tools".to_string(),
            serde_json::json!([
                {
                    "type": "function",
                    "name": "edit",
                    "description": "Edit file",
                    "parameters": { "type": "object" }
                }
            ]),
        );

        inject_opencode_agent_quartet_tools(&mut obj, TargetFormat::Responses);

        let tools = obj
            .get("tools")
            .and_then(Value::as_array)
            .expect("tools array");
        assert_eq!(tools.len(), 5);

        for tool in tools {
            assert_eq!(tool.get("strict"), Some(&Value::Bool(false)));
        }
    }
}
