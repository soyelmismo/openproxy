use super::common::{
    execute_warmup_ping, format_account_desc, is_any_weekly_model_detail_exhausted,
    is_session_window_ticking, is_weekly_quota_exhausted, query_provider_models_fallback,
};
use super::traits::{WarmupAccountContext, WarmupStrategy};
use crate::accounts::{self, Account};
use crate::error::Result;
use crate::ids::ProviderId;
use openproxy_adapters::adapters::claude_code::{
    CLAUDE_CODE_ANTHROPIC_VERSION, CLAUDE_CODE_BETA_HEADER, CLAUDE_CODE_USER_AGENT,
};
use openproxy_adapters::upstream::{UpstreamClient, UpstreamRequest};
use openproxy_db::secrets::MasterKey;
use openproxy_types::AccountQuota;
use sha2::{Digest, Sha256};
use std::fmt::Write;
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub struct ClaudeCodeWarmupStrategy;

impl ClaudeCodeWarmupStrategy {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ClaudeCodeWarmupStrategy {
    fn default() -> Self {
        Self::new()
    }
}

impl WarmupStrategy for ClaudeCodeWarmupStrategy {
    fn provider_id(&self) -> &'static str {
        "claude-code"
    }

    fn extract_account(
        &self,
        account: &Account,
        conn: &rusqlite::Connection,
        master_key: &MasterKey,
    ) -> Option<WarmupAccountContext> {
        let acc_id = account.id.0;
        let token = accounts::decrypt_access_token(conn, account.id, master_key).ok()?;
        let meta = account
            .oauth_provider_specific
            .as_deref()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
        let account_uuid = meta
            .as_ref()
            .and_then(|v| v.get("account_uuid").or_else(|| v.get("accountUuid")))
            .and_then(|v| v.as_str())
            .unwrap_or("53098eb0-6bbd-4c76-9f9c-09bc956164bc");

        let account_desc = format_account_desc(account);

        Some(WarmupAccountContext {
            account_id: acc_id,
            access_token: token,
            account_desc,
            extra: serde_json::json!({
                "account_uuid": account_uuid,
            }),
        })
    }

    async fn fetch_quota(
        &self,
        upstream: &Arc<UpstreamClient>,
        account: &WarmupAccountContext,
    ) -> Option<Result<AccountQuota>> {
        let adapter = openproxy_adapters::adapters::ProviderAdapterEnum::ClaudeCode(Box::new(
            openproxy_adapters::adapters::ClaudeCodeAdapter::new(),
        ));
        adapter
            .fetch_quota(upstream, "", Some(&account.access_token), None)
            .await
    }

    fn resolve_models(&self, conn: &rusqlite::Connection, config_models: &[String]) -> Vec<String> {
        let mut resolved = Vec::new();

        // 1. Check if configured model explicitly matches claude-code / claude
        for alias in config_models {
            if let Some(target) = resolve_claude_model(conn, alias)
                && !resolved.contains(&target)
            {
                resolved.push(target);
            }
        }

        // 2. Fallback: discover the newest active claude-code model
        if resolved.is_empty()
            && let Some(target) = resolve_default_claude_model(conn)
        {
            resolved.push(target);
        }

        resolved
    }

    fn is_quota_ready(&self, quota: &AccountQuota, _model_id: &str, now: i64) -> bool {
        // 1. If weekly quota is 100% exhausted, under NO circumstance attempt to warm up the 5h window
        if is_weekly_quota_exhausted(quota, now) {
            tracing::debug!(
                weekly_used = ?quota.weekly_used,
                weekly_limit = ?quota.weekly_limit,
                reset_at = ?quota.weekly_reset_at,
                "[SmartWarmup] Claude Code weekly quota is 100% utilized; skipping 5h window warmup"
            );
            return false;
        }

        // Also check weekly model details if present (e.g. Claude Sonnet (Weekly), Claude Opus (Weekly))
        if is_any_weekly_model_detail_exhausted(
            quota,
            |model_id| {
                let lower = model_id.to_lowercase();
                lower.contains("weekly") || lower.contains("seven_day")
            },
            now,
        ) {
            tracing::debug!(
                "[SmartWarmup] Claude Code weekly model detail is 100% utilized; skipping 5h window warmup"
            );
            return false;
        }

        // 2. If the 5-hour session window is already ticking with usage, do not ping
        if is_session_window_ticking(quota, now) {
            return false;
        }

        // 3. 5h window is completely fresh (0% used) and ready to kickstart
        quota.session_used.unwrap_or(0) == 0
    }

    async fn ping_model(
        &self,
        upstream: &Arc<UpstreamClient>,
        account: &WarmupAccountContext,
        model: &str,
    ) -> bool {
        let account_uuid = account
            .extra
            .get("account_uuid")
            .and_then(|v| v.as_str())
            .unwrap_or("53098eb0-6bbd-4c76-9f9c-09bc956164bc");

        let device_id = {
            let mut hasher = Sha256::new();
            hasher.update(account_uuid.as_bytes());
            hasher.update(b"openproxy-claude-device");
            let hash = hasher.finalize();
            hash.iter().fold(String::with_capacity(64), |mut s, b| {
                let _ = write!(s, "{b:02x}");
                s
            })
        };

        let session_id = uuid::Uuid::new_v4().to_string();
        let user_id_payload = serde_json::json!({
            "device_id": device_id,
            "account_uuid": account_uuid,
            "session_id": session_id,
        });

        let payload_json = serde_json::json!({
            "model": model,
            "max_tokens": 1,
            "messages": [
                {
                    "role": "user",
                    "content": "hi"
                }
            ],
            "metadata": {
                "user_id": user_id_payload.to_string(),
            }
        });

        let payload_bytes = match serde_json::to_vec(&payload_json) {
            Ok(b) => bytes::Bytes::from(b),
            Err(_) => return false,
        };

        let mut req =
            UpstreamRequest::post_json("https://api.anthropic.com/v1/messages", payload_bytes)
                .with_bearer(&account.access_token)
                .with_accept_json();

        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        req.headers.insert(
            http::header::HeaderName::from_static("anthropic-version"),
            http::HeaderValue::from_static(CLAUDE_CODE_ANTHROPIC_VERSION),
        );
        req.headers.insert(
            http::header::HeaderName::from_static("anthropic-beta"),
            http::HeaderValue::from_static(CLAUDE_CODE_BETA_HEADER),
        );
        req.headers.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static(CLAUDE_CODE_USER_AGENT),
        );

        execute_warmup_ping(upstream, req, account, "claude-code", model).await
    }
}

pub fn resolve_claude_model(conn: &rusqlite::Connection, alias: &str) -> Option<String> {
    let lower = alias.to_lowercase();
    if !lower.contains("claude")
        && !lower.contains("sonnet")
        && !lower.contains("haiku")
        && !lower.contains("opus")
    {
        return None;
    }

    if let Ok(Some(model)) = crate::models::find_active_by_provider_and_name(
        conn,
        &ProviderId::new("claude-code"),
        alias,
    ) {
        return Some(model.model_id.0);
    }

    resolve_default_claude_model(conn)
}

fn resolve_default_claude_model(conn: &rusqlite::Connection) -> Option<String> {
    query_provider_models_fallback(conn, "claude-code", |m| m.contains("sonnet"))
}
