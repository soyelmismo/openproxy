use crate::accounts::Account;
use crate::error::Result;
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_db::secrets::MasterKey;
use openproxy_types::AccountQuota;
use std::sync::Arc;

/// Decrypted account context and provider metadata needed for warmup execution.
#[derive(Clone, Debug)]
pub struct WarmupAccountContext {
    pub account_id: i64,
    pub access_token: String,
    pub account_desc: String,
    pub extra: serde_json::Value,
}

/// Unified abstraction for provider-specific quota warmup strategies.
pub trait WarmupStrategy: Send + Sync {
    /// Canonical provider identifier (e.g. "antigravity", "codex", "claude-code").
    fn provider_id(&self) -> &'static str;

    /// Extracts credentials and required metadata for an account. Returns `None` if
    /// the account lacks required fields or credentials cannot be decrypted.
    fn extract_account(
        &self,
        account: &Account,
        conn: &rusqlite::Connection,
        master_key: &MasterKey,
    ) -> Option<WarmupAccountContext>;

    /// Fetches the latest live quota for the account.
    fn fetch_quota<'a>(
        &'a self,
        upstream: &'a Arc<UpstreamClient>,
        account: &'a WarmupAccountContext,
    ) -> impl std::future::Future<Output = Option<Result<AccountQuota>>> + Send + 'a;

    /// Resolves target model IDs to evaluate for warmup against active DB models.
    fn resolve_models(&self, conn: &rusqlite::Connection, config_models: &[String]) -> Vec<String>;

    /// Returns `true` if the quota is full / unspent and ready to kickstart the reset window.
    /// Returns `false` if the window is already ticking or quota is exhausted.
    fn is_quota_ready(&self, quota: &AccountQuota, model_id: &str, now: i64) -> bool;

    /// Sends a minimal dummy request to upstream to kickstart the quota window.
    fn ping_model<'a>(
        &'a self,
        upstream: &'a Arc<UpstreamClient>,
        account: &'a WarmupAccountContext,
        model_id: &'a str,
    ) -> impl std::future::Future<Output = bool> + Send + 'a;
}

/// Enum wrapper providing zero-cost static dispatch across all built-in warmup strategies.
#[derive(Clone)]
pub enum WarmupStrategyEnum {
    Antigravity(crate::smart_warmup::antigravity::AntigravityWarmupStrategy),
    Codex(crate::smart_warmup::codex::CodexWarmupStrategy),
    ClaudeCode(crate::smart_warmup::claude_code::ClaudeCodeWarmupStrategy),
}

impl WarmupStrategy for WarmupStrategyEnum {
    fn provider_id(&self) -> &'static str {
        match self {
            Self::Antigravity(s) => s.provider_id(),
            Self::Codex(s) => s.provider_id(),
            Self::ClaudeCode(s) => s.provider_id(),
        }
    }

    fn extract_account(
        &self,
        account: &Account,
        conn: &rusqlite::Connection,
        master_key: &MasterKey,
    ) -> Option<WarmupAccountContext> {
        match self {
            Self::Antigravity(s) => s.extract_account(account, conn, master_key),
            Self::Codex(s) => s.extract_account(account, conn, master_key),
            Self::ClaudeCode(s) => s.extract_account(account, conn, master_key),
        }
    }

    async fn fetch_quota(
        &self,
        upstream: &Arc<UpstreamClient>,
        account: &WarmupAccountContext,
    ) -> Option<Result<AccountQuota>> {
        match self {
            Self::Antigravity(s) => s.fetch_quota(upstream, account).await,
            Self::Codex(s) => s.fetch_quota(upstream, account).await,
            Self::ClaudeCode(s) => s.fetch_quota(upstream, account).await,
        }
    }

    fn resolve_models(&self, conn: &rusqlite::Connection, config_models: &[String]) -> Vec<String> {
        match self {
            Self::Antigravity(s) => s.resolve_models(conn, config_models),
            Self::Codex(s) => s.resolve_models(conn, config_models),
            Self::ClaudeCode(s) => s.resolve_models(conn, config_models),
        }
    }

    fn is_quota_ready(&self, quota: &AccountQuota, model_id: &str, now: i64) -> bool {
        match self {
            Self::Antigravity(s) => s.is_quota_ready(quota, model_id, now),
            Self::Codex(s) => s.is_quota_ready(quota, model_id, now),
            Self::ClaudeCode(s) => s.is_quota_ready(quota, model_id, now),
        }
    }

    async fn ping_model(
        &self,
        upstream: &Arc<UpstreamClient>,
        account: &WarmupAccountContext,
        model_id: &str,
    ) -> bool {
        match self {
            Self::Antigravity(s) => s.ping_model(upstream, account, model_id).await,
            Self::Codex(s) => s.ping_model(upstream, account, model_id).await,
            Self::ClaudeCode(s) => s.ping_model(upstream, account, model_id).await,
        }
    }
}

/// Returns `true` if `reset_str` is an RFC3339 timestamp strictly in the future.
pub fn is_future_reset(reset_str: Option<&str>, now: i64) -> bool {
    reset_str
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .is_some_and(|dt| dt.timestamp() > now)
}
