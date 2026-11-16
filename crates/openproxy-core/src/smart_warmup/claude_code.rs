use super::common::{
    execute_warmup_ping, format_account_desc, is_5h_session_window_ready,
    query_provider_models_fallback, resolve_strategy_models,
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

#[derive(Clone, Copy, Debug, Default)]
pub struct ClaudeCodeWarmupStrategy;

impl ClaudeCodeWarmupStrategy {
    pub fn new() -> Self {
        Self
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
        resolve_strategy_models(
            conn,
            config_models,
            resolve_claude_model,
            resolve_default_claude_model,
        )
    }

    fn is_quota_ready(&self, quota: &AccountQuota, _model_id: &str, now: i64) -> bool {
        is_5h_session_window_ready(
            quota,
            |model_id| {
                let lower = model_id.to_lowercase();
                lower.contains("weekly") || lower.contains("seven_day")
            },
            "Claude Code",
            now,
        )
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

        // The warmup mirrors a real streaming client request so the upstream
        // sees the same traffic shape; keep the streaming transport.
        req.is_streaming = true;
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
