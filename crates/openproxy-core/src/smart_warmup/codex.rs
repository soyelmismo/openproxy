use super::common::{
    execute_warmup_ping, format_account_desc, is_any_weekly_model_detail_exhausted,
    is_session_window_ticking, is_weekly_quota_exhausted, query_provider_models_fallback,
};
use super::traits::{WarmupAccountContext, WarmupStrategy, is_future_reset};
use crate::accounts::{self, Account};
use crate::error::Result;
use crate::ids::ProviderId;
use openproxy_adapters::adapters::codex::{
    apply_codex_spoofing_headers, quota::codex_workspace_header,
};
use openproxy_adapters::upstream::{UpstreamClient, UpstreamRequest};
use openproxy_db::secrets::MasterKey;
use openproxy_types::AccountQuota;
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub struct CodexWarmupStrategy;

impl CodexWarmupStrategy {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CodexWarmupStrategy {
    fn default() -> Self {
        Self::new()
    }
}

impl WarmupStrategy for CodexWarmupStrategy {
    fn provider_id(&self) -> &'static str {
        "codex"
    }

    fn extract_account(
        &self,
        account: &Account,
        conn: &rusqlite::Connection,
        master_key: &MasterKey,
    ) -> Option<WarmupAccountContext> {
        let acc_id = account.id.0;
        let token = accounts::decrypt_access_token(conn, account.id, master_key).ok()?;
        let meta = account.oauth_provider_specific.as_deref();
        let workspace_id = meta.and_then(codex_workspace_header);
        let account_desc = format_account_desc(account);

        Some(WarmupAccountContext {
            account_id: acc_id,
            access_token: token,
            account_desc,
            extra: serde_json::json!({
                "workspace_id": workspace_id,
            }),
        })
    }

    async fn fetch_quota(
        &self,
        upstream: &Arc<UpstreamClient>,
        account: &WarmupAccountContext,
    ) -> Option<Result<AccountQuota>> {
        let workspace_id = account.extra.get("workspace_id").and_then(|v| v.as_str());
        let adapter = openproxy_adapters::adapters::ProviderAdapterEnum::Codex(Box::new(
            openproxy_adapters::adapters::CodexAdapter::new(),
        ));
        adapter
            .fetch_quota(upstream, "", Some(&account.access_token), workspace_id)
            .await
    }

    fn resolve_models(&self, conn: &rusqlite::Connection, config_models: &[String]) -> Vec<String> {
        let mut resolved = Vec::new();

        // 1. Check if any configured model explicitly targets codex / gpt
        for alias in config_models {
            if let Some(target) = resolve_codex_model(conn, alias)
                && !resolved.contains(&target)
            {
                resolved.push(target);
            }
        }

        // 2. Fallback: if no config models matched codex, discover the newest active codex model
        if resolved.is_empty()
            && let Some(target) = resolve_default_codex_model(conn)
        {
            resolved.push(target);
        }

        resolved
    }

    fn is_quota_ready(&self, quota: &AccountQuota, model_id: &str, now: i64) -> bool {
        // 1. Weekly window exhausted: under NO circumstance attempt to warm up session window
        if is_weekly_quota_exhausted(quota, now) {
            tracing::debug!(
                weekly_used = ?quota.weekly_used,
                weekly_limit = ?quota.weekly_limit,
                reset_at = ?quota.weekly_reset_at,
                "[SmartWarmup] Codex weekly quota is 100% utilized; skipping session window warmup"
            );
            return false;
        }

        // Also check additional rate limits for weekly limits
        if is_any_weekly_model_detail_exhausted(
            quota,
            |model_id| {
                let lower = model_id.to_lowercase();
                lower.contains("weekly") || lower.contains("secondary")
            },
            now,
        ) {
            return false;
        }

        if let Some(details) = &quota.model_details {
            // Specific model rate limit already ticking?
            let matched = details.iter().find(|d| d.model_id == model_id);
            if let Some(d) = matched
                && d.session_used > 0
                && is_future_reset(d.session_reset_at.as_deref(), now)
            {
                return false;
            }
            if let Some(d) = matched {
                return d.session_used == 0 && d.remaining_fraction >= 0.999;
            }
        }

        // 2. Primary session window already ticking?
        if is_session_window_ticking(quota, now) {
            return false;
        }

        // 3. Ready if session_used is 0
        quota.session_used.unwrap_or(0) == 0
    }

    async fn ping_model(
        &self,
        upstream: &Arc<UpstreamClient>,
        account: &WarmupAccountContext,
        model: &str,
    ) -> bool {
        let workspace_id = account.extra.get("workspace_id").and_then(|v| v.as_str());

        let payload_json = serde_json::json!({
            "model": model,
            "input": [
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "input_text",
                            "text": "hi"
                        }
                    ]
                }
            ],
            "stream": true,
            "store": false
        });

        let payload_bytes = match serde_json::to_vec(&payload_json) {
            Ok(b) => bytes::Bytes::from(b),
            Err(_) => return false,
        };

        let mut req = UpstreamRequest::post_json(
            "https://chatgpt.com/backend-api/codex/responses",
            payload_bytes,
        )
        .with_bearer(&account.access_token);

        req.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("text/event-stream"),
        );
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );

        apply_codex_spoofing_headers(&mut req);

        if let Ok(hint) = http::HeaderValue::from_str(&format!("model={model}")) {
            req.headers
                .insert(http::HeaderName::from_static("x-codex-routing-hint"), hint);
        }

        if let Some(ws) = workspace_id
            && let Ok(ws_val) = http::HeaderValue::from_str(ws)
        {
            req.headers
                .insert(http::HeaderName::from_static("chatgpt-account-id"), ws_val);
        }

        execute_warmup_ping(upstream, req, account, "codex", model).await
    }
}

pub fn resolve_codex_model(conn: &rusqlite::Connection, alias: &str) -> Option<String> {
    let lower = alias.to_lowercase();
    if !lower.contains("codex") && !lower.contains("gpt") && !lower.contains("openai") {
        return None;
    }

    if let Ok(Some(model)) =
        crate::models::find_active_by_provider_and_name(conn, &ProviderId::new("codex"), alias)
    {
        return Some(model.model_id.0);
    }

    resolve_default_codex_model(conn)
}

fn resolve_default_codex_model(conn: &rusqlite::Connection) -> Option<String> {
    query_provider_models_fallback(conn, "codex", |m| {
        m.contains("gpt-5") || m.contains("gpt-6")
    })
}
