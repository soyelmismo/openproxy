use super::traits::{WarmupAccountContext, WarmupStrategy, is_future_reset};
use crate::accounts::{self, Account};
use crate::error::Result;
use crate::ids::ProviderId;
use openproxy_adapters::adapters::claude_code::{
    CLAUDE_CODE_ANTHROPIC_VERSION, CLAUDE_CODE_BETA_HEADER, CLAUDE_CODE_USER_AGENT,
};
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
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

        let acc_id_str = acc_id.to_string();
        let label = account
            .label
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != acc_id_str);
        let email = account
            .email
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
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
        if let Some(weekly_used) = quota.weekly_used {
            let weekly_limit = quota.weekly_limit.unwrap_or(100);
            if weekly_used >= weekly_limit
                && quota
                    .weekly_reset_at
                    .as_deref()
                    .is_none_or(|r| is_future_reset(Some(r), now))
            {
                tracing::debug!(
                    weekly_used,
                    weekly_limit,
                    reset_at = ?quota.weekly_reset_at,
                    "[SmartWarmup] Claude Code weekly quota is 100% utilized; skipping 5h window warmup"
                );
                return false;
            }
        }

        // Also check weekly model details if present (e.g. Claude Sonnet (Weekly), Claude Opus (Weekly))
        if let Some(details) = &quota.model_details {
            for detail in details {
                let lower = detail.model_id.to_lowercase();
                if lower.contains("weekly") || lower.contains("seven_day") {
                    let limit = if detail.session_limit > 0 {
                        detail.session_limit
                    } else {
                        100
                    };
                    if (detail.session_used >= limit || detail.remaining_fraction <= 0.001)
                        && detail
                            .session_reset_at
                            .as_deref()
                            .is_none_or(|r| is_future_reset(Some(r), now))
                    {
                        tracing::debug!(
                            model = %detail.model_id,
                            used = detail.session_used,
                            "[SmartWarmup] Claude Code weekly model detail is 100% utilized; skipping 5h window warmup"
                        );
                        return false;
                    }
                }
            }
        }

        // 2. If the 5-hour session window is already ticking with usage, do not ping
        if let Some(session_used) = quota.session_used
            && session_used > 0
            && is_future_reset(quota.session_reset_at.as_deref(), now)
        {
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
            UpstreamRequest::post_json("https://api.anthropic.com/v1/messages", payload_bytes);

        if let Ok(val) = http::HeaderValue::from_str(&format!("Bearer {}", account.access_token)) {
            req.headers.insert(http::header::AUTHORIZATION, val);
        }
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        req.headers.insert(
            http::header::ACCEPT,
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

        let cancel = CancellationToken::new();
        match upstream
            .call(req, TimeoutProfile::ModelDiscovery, cancel)
            .await
        {
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
                    provider = "claude-code",
                    account_id = account.account_id,
                    model = %model,
                    status = %status,
                    "[SmartWarmup] Ping failed on Claude Code for model '{}' on account {}: {}",
                    model,
                    account.account_desc,
                    snippet
                );
            }
            Err(e) => {
                tracing::warn!(
                    provider = "claude-code",
                    account_id = account.account_id,
                    model = %model,
                    error = %e,
                    "[SmartWarmup] Ping request error on Claude Code for model '{}' on account {}: {}",
                    model,
                    account.account_desc,
                    e
                );
            }
        }

        false
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
    let mut stmt = conn
        .prepare(
            "SELECT model_id FROM models \
             WHERE provider_id = 'claude-code' AND active = 1 \
             ORDER BY model_id DESC",
        )
        .ok()?;

    let mut models: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .ok()?
        .filter_map(|r| r.ok())
        .collect();

    // Prefer Sonnet
    if let Some(m) = models.iter().find(|m| m.contains("sonnet")) {
        return Some(m.clone());
    }

    models.pop()
}
