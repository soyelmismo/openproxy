use super::*;

const DEFAULT_CODEX_CLIENT_VERSION: &str = "0.142.0";

fn safe_env_value(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

pub fn codex_client_version() -> String {
    safe_env_value("OPENPROXY_CODEX_CLIENT_VERSION")
        .or_else(|| safe_env_value("CODEX_CLIENT_VERSION"))
        .unwrap_or_else(|| DEFAULT_CODEX_CLIENT_VERSION.to_string())
}

pub fn codex_user_agent() -> String {
    safe_env_value("OPENPROXY_CODEX_USER_AGENT")
        .or_else(|| safe_env_value("CODEX_USER_AGENT"))
        .unwrap_or_else(|| {
            format!(
                "codex-cli/{} (Windows 10.0.26200; x64)",
                codex_client_version()
            )
        })
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CodexAdapter {
    config: ProviderAdapterConfig,
}

impl CodexAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("codex"),
                base_url: "https://chatgpt.com/backend-api/codex".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Responses,
                extra_headers: vec![],
            },
        }
    }

    fn hardcoded_models(&self) -> Vec<DiscoveredModel> {
        [
            ("gpt-5.5", "GPT-5.5"),
            ("gpt-5.5-xhigh", "GPT-5.5 (xhigh)"),
            ("gpt-5.5-high", "GPT-5.5 (high)"),
            ("gpt-5.5-medium", "GPT-5.5 (medium)"),
            ("gpt-5.5-low", "GPT-5.5 (low)"),
            ("gpt-5.4", "GPT-5.4"),
            ("gpt-5.4-xhigh", "GPT-5.4 (xhigh)"),
            ("gpt-5.4-high", "GPT-5.4 (high)"),
            ("gpt-5.4-medium", "GPT-5.4 (medium)"),
            ("gpt-5.4-low", "GPT-5.4 (low)"),
            ("gpt-5.4-mini", "GPT-5.4 Mini"),
            ("gpt-5.3-codex", "GPT-5.3 Codex"),
            ("gpt-5.3-codex-spark", "GPT-5.3 Codex Spark"),
        ]
        .into_iter()
        .map(|(id, name)| DiscoveredModel {
            model_id: ModelId::new(id),
            display_name: Some(name.to_string()),
            target_format: TargetFormat::Responses,
            context_length: Some(400_000),
            max_output_tokens: Some(32_768),
            input_modalities: None,
            output_modalities: None,
            model_type: Some("chat".to_string()),
            family: Some("gpt".to_string()),
            capabilities: Some(openproxy_types::ModelCapabilities {
                vision: Some(false),
                tool_calling: Some(true),
                reasoning: Some(true),
                thinking: Some(true),
                attachment: None,
                structured_output: None,
                temperature: None,
            }),
        })
        .collect()
    }
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderAdapter for CodexAdapter {
    fn id(&self) -> &ProviderId {
        &self.config.id
    }

    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn metadata(&self) -> openproxy_types::ProviderMetadata {
        openproxy_types::ProviderMetadata {
            built_in: openproxy_types::is_builtin(self.id().as_str()),
            deletable: !openproxy_types::is_builtin(self.id().as_str()),
            supports_quota: true,
            quota_refresh_supported: true,
            requires_oauth: true,
            oauth_refresh_lead_seconds: Some(300),
        }
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        format!("{}/responses", self.config.base_url)
    }

    fn build_auth_header(&self, api_key: &str) -> Option<(String, String)> {
        Some(("Authorization".into(), format!("Bearer {}", api_key)))
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let mut headers = vec![
            ("Content-Type".into(), "application/json".into()),
            ("Origin".into(), "https://chatgpt.com".into()),
            ("originator".into(), "codex_cli_rs".into()),
            (
                "Version".into(),
                codex_client_version(),
            ),
            (
                "User-Agent".into(),
                codex_user_agent(),
            ),
        ];
        if let Some(auth) = self.build_auth_header(api_key) {
            headers.push(auth);
        }
        headers
    }

    fn models_url(&self) -> Option<String> {
        None
    }

    async fn fetch_models(
        &self,
        _upstream_client: &Arc<UpstreamClient>,
        _api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        Ok(self.hardcoded_models())
    }

    async fn fetch_quota(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        _: &str,
        access_token: Option<&str>,
        provider_specific: Option<&str>,
    ) -> Option<Result<openproxy_types::AccountQuota>> {
        if let Some(token) = access_token {
            Some(self.fetch_codex_quota_local(upstream_client, token, provider_specific).await)
        } else {
            Some(Ok(openproxy_types::AccountQuota {
                session_used: None,
                session_limit: None,
                session_reset_at: None,
                weekly_used: None,
                weekly_limit: None,
                weekly_reset_at: None,
                plan_name: None,
                last_fetched_at: openproxy_types::now_unix_secs_str(),
                fetch_error: Some("codex requires OAuth access token".into()),
                model_details: None,
            }))
        }
    }
}

impl CodexAdapter {
    async fn fetch_codex_quota_local(
        &self,
        upstream: &Arc<UpstreamClient>,
        access_token: &str,
        workspace_id: Option<&str>,
    ) -> Result<openproxy_types::AccountQuota> {
        let url = "https://chatgpt.com/backend-api/wham/usage";
        let mut req = UpstreamRequest::get(url);
        req.headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_str(&format!("Bearer {}", access_token))
                .unwrap_or_else(|_| http::HeaderValue::from_static("")),
        );
        req.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json"),
        );
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        req.headers.insert(
            http::header::HeaderName::from_static("origin"),
            http::HeaderValue::from_static("https://chatgpt.com"),
        );
        req.headers.insert(
            http::header::HeaderName::from_static("originator"),
            http::HeaderValue::from_static("codex_cli_rs"),
        );
        if let Ok(v) = http::HeaderValue::from_str(&codex_client_version()) {
            req.headers
                .insert(http::HeaderName::from_static("version"), v);
        }
        if let Ok(v) = http::HeaderValue::from_str(&codex_user_agent()) {
            req.headers.insert(http::header::USER_AGENT, v);
        }
        let workspace_header = workspace_id.and_then(codex_workspace_header);
        if let Some(ws) = workspace_header.as_deref()
            && let Ok(val) = http::HeaderValue::from_str(ws)
        {
            req.headers
                .insert(http::HeaderName::from_static("chatgpt-account-id"), val);
        }

        let cancel = CancellationToken::new();
        let response = upstream
            .call(req, TimeoutProfile::Chat, cancel)
            .await
            .map_err(|e| CoreError::UpstreamConnection(e.to_string()))?;

        let status = response.status.as_u16();
        if !(200..300).contains(&status) {
            let body = response.collect().await.unwrap_or_default();
            let snippet = String::from_utf8_lossy(&body)
                .chars()
                .take(200)
                .collect::<String>();
            return Ok(openproxy_types::AccountQuota {
                session_used: None,
                session_limit: None,
                session_reset_at: None,
                weekly_used: None,
                weekly_limit: None,
                weekly_reset_at: None,
                plan_name: None,
                last_fetched_at: openproxy_types::now_unix_secs_str(),
                fetch_error: Some(if snippet.is_empty() {
                    format!("Codex quota check failed: HTTP {}", status)
                } else {
                    format!("Codex quota check failed: HTTP {}: {}", status, snippet)
                }),
                model_details: None,
            });
        }

        let body = response
            .collect()
            .await
            .map_err(|e| CoreError::UpstreamConnection(format!("codex quota read: {e}")))?;
        let json: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| CoreError::Parse(format!("codex quota parse: {e}")))?;
        parse_codex_usage_quota(&json)
    }
}

fn codex_workspace_header(provider_specific: &str) -> Option<String> {
    let raw = provider_specific.trim();
    if raw.is_empty() {
        return None;
    }
    if !raw.starts_with('{') {
        return Some(raw.to_string());
    }
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| {
            v.get("workspaceId")
                .or_else(|| v.get("workspace_id"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
        })
}

fn parse_codex_usage_quota(body: &serde_json::Value) -> Result<openproxy_types::AccountQuota> {
    let rate_limit = body
        .get("rate_limit")
        .or_else(|| body.get("rateLimit"))
        .and_then(|v| v.as_object())
        .ok_or_else(|| CoreError::Parse("codex quota missing rate_limit".into()))?;

    let primary = rate_limit
        .get("primary_window")
        .or_else(|| rate_limit.get("primaryWindow"));
    let secondary = rate_limit
        .get("secondary_window")
        .or_else(|| rate_limit.get("secondaryWindow"));
    let (session_used, session_reset_at) = parse_codex_usage_window(primary);
    let (weekly_used, weekly_reset_at) = parse_codex_usage_window(secondary);

    Ok(openproxy_types::AccountQuota {
        session_used,
        session_limit: session_used.map(|_| 100),
        session_reset_at,
        weekly_used,
        weekly_limit: weekly_used.map(|_| 100),
        weekly_reset_at,
        plan_name: Some("Codex / ChatGPT".into()),
        last_fetched_at: openproxy_types::now_unix_secs_str(),
        fetch_error: None,
        model_details: None,
    })
}

fn parse_codex_usage_window(window: Option<&serde_json::Value>) -> (Option<i64>, Option<String>) {
    let Some(window) = window.and_then(|v| v.as_object()) else {
        return (None, None);
    };
    let used = window
        .get("used_percent")
        .or_else(|| window.get("usedPercent"))
        .and_then(json_f64)
        .map(|v| v.round().clamp(0.0, 100.0) as i64);
    let reset_at = window
        .get("reset_at")
        .or_else(|| window.get("resetAt"))
        .and_then(json_f64)
        .filter(|v| *v > 0.0)
        .map(|v| (v.ceil() as u64).to_string())
        .or_else(|| {
            window
                .get("reset_after_seconds")
                .or_else(|| window.get("resetAfterSeconds"))
                .and_then(json_f64)
                .filter(|v| *v > 0.0)
                .map(|v| {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    (now + v.ceil() as u64).to_string()
                })
        });
    (used, reset_at)
}

fn json_f64(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse::<f64>().ok()))
}
