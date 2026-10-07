use super::traits::{WarmupAccountContext, WarmupStrategy, is_future_reset};
use crate::accounts::{self, Account};
use crate::error::Result;
use crate::ids::ProviderId;
use openproxy_adapters::upstream::{CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest};
use openproxy_db::secrets::MasterKey;
use openproxy_types::{AccountQuota, OpenAIMessage, OpenAIRequest};
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub struct AntigravityWarmupStrategy;

impl AntigravityWarmupStrategy {
    pub fn new() -> Self {
        Self
    }
}

impl Default for AntigravityWarmupStrategy {
    fn default() -> Self {
        Self::new()
    }
}

pub fn build_warmup_request(model: &str) -> OpenAIRequest {
    let lower = model.to_lowercase();
    let is_gemini = lower.contains("gemini");

    let (prompt, max_tokens, temperature) = if is_gemini {
        (
            "Write a complete, detailed Python module with functions to compute Fibonacci numbers, check for prime numbers, and calculate the greatest common divisor using Euclidean algorithm. Include docstrings, type annotations, and full unit tests for all functions.",
            Some(1024),
            Some(0.2),
        )
    } else {
        ("Say hi", None, Some(0.0))
    };

    OpenAIRequest {
        model: model.to_string(),
        messages: vec![OpenAIMessage {
            role: "user".to_string(),
            content: Some(serde_json::Value::String(prompt.to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::new(),
        }],
        max_tokens,
        temperature,
        stream: false,
        top_p: None,
        stop: None,
        tools: None,
        tool_choice: None,
        top_k: None,
        user: None,
        extra: serde_json::Map::new(),
    }
}

impl WarmupStrategy for AntigravityWarmupStrategy {
    fn provider_id(&self) -> &'static str {
        "antigravity"
    }

    fn extract_account(
        &self,
        account: &Account,
        conn: &rusqlite::Connection,
        master_key: &MasterKey,
    ) -> Option<WarmupAccountContext> {
        let acc_id = account.id.0;
        let token = accounts::decrypt_access_token(conn, account.id, master_key).ok()?;
        let meta = account.oauth_provider_specific.as_deref()?;
        let v: serde_json::Value = serde_json::from_str(meta).ok()?;
        let project_id = openproxy_pipeline::credentials::antigravity_project_from_value(&v)?;
        let acc_id_str = acc_id.to_string();
        let label = account
            .label
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != acc_id_str);
        let email = account.email.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let account_desc = match (label, email) {
            (Some(l), Some(e)) => format!("{acc_id} ({l} / {e})"),
            (Some(l), None) => format!("{acc_id} ({l})"),
            (None, Some(e)) => format!("{acc_id} ({e})"),
            (None, None) => acc_id_str,
        };

        Some(WarmupAccountContext {
            account_id: acc_id,
            access_token: token,
            account_desc,
            extra: serde_json::json!({ "project_id": project_id }),
        })
    }

    async fn fetch_quota(
        &self,
        upstream: &Arc<UpstreamClient>,
        account: &WarmupAccountContext,
    ) -> Option<Result<AccountQuota>> {
        let project_id = account
            .extra
            .get("project_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let adapter = openproxy_adapters::adapters::ProviderAdapterEnum::Antigravity(Box::new(
            openproxy_adapters::adapters::AntigravityAdapter::new(),
        ));
        adapter
            .fetch_quota(upstream, project_id, Some(&account.access_token), None)
            .await
    }

    fn resolve_models(
        &self,
        conn: &rusqlite::Connection,
        config_models: &[String],
    ) -> Vec<String> {
        let mut resolved = Vec::new();
        for alias in config_models {
            if let Some(target) = resolve_antigravity_model(conn, alias)
                && !resolved.contains(&target)
            {
                resolved.push(target);
            }
        }
        resolved
    }

    fn is_quota_ready(&self, quota: &AccountQuota, true_model_id: &str, now: i64) -> bool {
        // Global weekly quota 100% exhausted? (Antigravity operates on base 1000)
        if let Some(weekly_used) = quota.weekly_used {
            let weekly_limit = quota.weekly_limit.unwrap_or(1000);
            if weekly_used >= weekly_limit
                && quota
                    .weekly_reset_at
                    .as_deref()
                    .is_none_or(|r| is_future_reset(Some(r), now))
            {
                return false;
            }
        }

        let lower_target = true_model_id.to_lowercase();
        let is_claude = lower_target.contains("claude");
        let is_gemini = lower_target.contains("gemini");

        if is_claude && let Some(details) = &quota.model_details {
            let weekly_detail = details.iter().find(|d| d.model_id == "Claude (Weekly)");
            let session_detail = details
                .iter()
                .find(|d| d.model_id == "Claude (5h)")
                .or_else(|| details.iter().find(|d| d.model_id == true_model_id));

            if let Some(w) = weekly_detail {
                let limit = if w.session_limit > 0 { w.session_limit } else { 1000 };
                if (w.session_used >= limit || w.remaining_fraction <= 0.001)
                    && w.session_reset_at
                        .as_deref()
                        .is_none_or(|r| is_future_reset(Some(r), now))
                {
                    return false;
                }
            }

            if let Some(s) = session_detail {
                if s.session_used > 0 && is_future_reset(s.session_reset_at.as_deref(), now) {
                    return false;
                }
                return s.session_used == 0 && s.remaining_fraction >= 0.999;
            }
        }

        if is_gemini {
            if let Some(details) = &quota.model_details {
                let matched = details.iter().find(|d| d.model_id == true_model_id);
                if let Some(d) = matched {
                    if d.session_used > 0 && is_future_reset(d.session_reset_at.as_deref(), now) {
                        return false;
                    }
                    return d.session_used == 0 && d.remaining_fraction >= 0.999;
                }
            }

            if let Some(used) = quota.session_used
                && used > 0
                && is_future_reset(quota.session_reset_at.as_deref(), now)
            {
                return false;
            }

            return quota.session_used.unwrap_or(0) == 0;
        }

        if let Some(details) = &quota.model_details
            && let Some(detail) = details.iter().find(|d| d.model_id == true_model_id)
        {
            if detail.session_used > 0 && is_future_reset(detail.session_reset_at.as_deref(), now) {
                return false;
            }
            return detail.session_used == 0 && detail.remaining_fraction >= 0.999;
        }

        if quota.session_used.unwrap_or(0) > 0
            && is_future_reset(quota.session_reset_at.as_deref(), now)
        {
            return false;
        }

        quota.session_used == Some(0)
    }

    async fn ping_model(
        &self,
        upstream: &Arc<UpstreamClient>,
        account: &WarmupAccountContext,
        model: &str,
    ) -> bool {
        let project_id = account
            .extra
            .get("project_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let physical_model =
            openproxy_adapters::adapters::antigravity::map_antigravity_physical_model(model);
        let request = build_warmup_request(model);
        let request_payload = serde_json::to_value(
            openproxy_adapters::adapters::gemini::openai_to_gemini(&request, &request.messages),
        )
        .unwrap_or_else(|_| serde_json::json!({}));

        let wrapped = serde_json::json!({
            "project": project_id,
            "model": physical_model,
            "requestType": "agent",
            "requestId": uuid::Uuid::new_v4().to_string(),
            "userAgent": "antigravity",
            "request": request_payload,
            "enabledCreditTypes": ["GOOGLE_ONE_AI"]
        });

        let payload = match serde_json::to_vec(&wrapped) {
            Ok(b) => bytes::Bytes::from(b),
            Err(_) => return false,
        };

        let endpoints = [
            "https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse",
            "https://cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse",
        ];

        for url in &endpoints {
            let mut req = UpstreamRequest::post_json(*url, payload.clone());
            if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {}", account.access_token)) {
                req.headers.insert(http::header::AUTHORIZATION, v);
            }
            openproxy_adapters::antigravity_headers::inject_antigravity_headers(
                &mut req.headers,
                None,
            );

            let cancel = CancellationToken::new();
            match upstream.call(req, TimeoutProfile::ModelDiscovery, cancel).await {
                Ok(resp) => {
                    let status = resp.status;
                    let body = resp.collect().await.unwrap_or_default();
                    if status.is_success() {
                        return true;
                    }
                    let body_str = String::from_utf8_lossy(&body);
                    let snippet = if body_str.len() > 200 {
                        let boundary = body_str
                            .char_indices()
                            .map(|(i, _)| i)
                            .take_while(|&i| i <= 200)
                            .last()
                            .unwrap_or(0);
                        &body_str[..boundary]
                    } else {
                        &body_str
                    };
                    tracing::warn!(
                        provider = "antigravity",
                        account_id = account.account_id,
                        model = %model,
                        status = %status,
                        endpoint = %url,
                        response = %snippet,
                        "[SmartWarmup] Ping failed with status {} on '{}' for model '{}' on account {}: {}",
                        status,
                        url,
                        model,
                        account.account_desc,
                        snippet
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        provider = "antigravity",
                        account_id = account.account_id,
                        model = %model,
                        endpoint = %url,
                        error = %e,
                        "[SmartWarmup] Ping request failed on '{}' for model '{}' on account {}: {}",
                        url,
                        model,
                        account.account_desc,
                        e
                    );
                }
            }
        }

        false
    }
}

pub fn resolve_antigravity_model(conn: &rusqlite::Connection, alias: &str) -> Option<String> {
    if let Some(exact) = resolve_exact_alias(conn, alias) {
        return Some(exact);
    }

    let lower = alias.to_lowercase();
    let is_claude = lower.contains("claude");
    let is_gemini = lower.contains("gemini");
    let wants_pro = lower.contains("pro");

    if !is_claude && !is_gemini {
        return None;
    }

    let mut stmt = conn
        .prepare(
            "SELECT model_id FROM models \
             WHERE provider_id = 'antigravity' AND active = 1 \
             ORDER BY model_id DESC",
        )
        .ok()?;

    let models: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .ok()?
        .filter_map(|r| r.ok())
        .collect();

    if is_claude {
        if let Some(m) = models
            .iter()
            .find(|m| m.contains("claude") && m.contains("sonnet"))
        {
            return Some(m.clone());
        }
        if let Some(m) = models.iter().find(|m| m.contains("claude")) {
            return Some(m.clone());
        }
    }

    if is_gemini {
        if wants_pro {
            if let Some(m) = models.iter().find(|m| {
                m.contains("gemini")
                    && m.contains("pro")
                    && (m.contains("high") || m.contains("agent"))
            }) {
                return Some(m.clone());
            }
            if let Some(m) = models.iter().find(|m| {
                m.contains("gemini")
                    && m.contains("pro")
                    && !m.contains("low")
                    && !m.contains("2.5")
            }) {
                return Some(m.clone());
            }
            if let Some(m) = models
                .iter()
                .find(|m| m.contains("gemini") && m.contains("pro"))
            {
                return Some(m.clone());
            }
        }
        if let Some(m) = models
            .iter()
            .find(|m| m.contains("gemini") && m.contains("flash") && m.contains("low"))
        {
            return Some(m.clone());
        }
        if let Some(m) = models.iter().find(|m| m.contains("gemini")) {
            return Some(m.clone());
        }
    }

    None
}

fn resolve_exact_alias(conn: &rusqlite::Connection, alias: &str) -> Option<String> {
    if let Ok(Some(combo)) = openproxy_db::combos::get_combo_by_name(conn, alias) {
        let mut visited = Vec::new();
        if let Ok(targets) = openproxy_pipeline::repository::resolve_combo_to_targets(
            conn,
            combo.id,
            &mut visited,
            0,
        ) {
            for target in targets {
                if let Some(row_id) = target.model_row_id
                    && let Ok(Some(model)) = crate::models::get_by_row_id(conn, row_id)
                    && model.provider_id.as_str() == "antigravity"
                {
                    return Some(model.model_id.0);
                }
            }
        }
    }

    if let Ok(Some(model)) = crate::models::find_active_by_provider_and_name(
        conn,
        &ProviderId::new("antigravity"),
        alias,
    ) {
        return Some(model.model_id.0);
    }

    None
}
