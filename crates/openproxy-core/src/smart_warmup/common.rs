use super::traits::{WarmupAccountContext, is_future_reset};
use crate::accounts::Account;
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use openproxy_types::AccountQuota;
use std::sync::Arc;

/// Formats a readable human account description `"<id> (<label> / <email>)"` fallbacking to `"<id>"`.
pub fn format_account_desc(account: &Account) -> String {
    let acc_id = account.id.0;
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
    match (label, email) {
        (Some(l), Some(e)) => format!("{acc_id} ({l} / {e})"),
        (Some(l), None) => format!("{acc_id} ({l})"),
        (None, Some(e)) => format!("{acc_id} ({e})"),
        (None, None) => acc_id_str,
    }
}

/// Helper to query active models for a provider ordered by model_id DESC and select one
/// matching `preference` predicate, or fallback to the latest.
pub fn query_provider_models_fallback<F>(
    conn: &rusqlite::Connection,
    provider_id: &str,
    preference: F,
) -> Option<String>
where
    F: Fn(&str) -> bool,
{
    let mut stmt = conn
        .prepare(
            "SELECT model_id FROM models \
             WHERE provider_id = ?1 AND active = 1 \
             ORDER BY model_id DESC",
        )
        .ok()?;

    let mut models: Vec<String> = stmt
        .query_map(rusqlite::params![provider_id], |row| row.get(0))
        .ok()?
        .filter_map(|r| r.ok())
        .collect();

    if let Some(m) = models.iter().find(|m| preference(m)) {
        return Some(m.clone());
    }

    models.pop()
}

/// Checks whether weekly quota is exhausted or session quota is currently ticking.
/// Returns `true` if weekly quota is 100% used (blocking warmup).
pub fn is_weekly_quota_exhausted(quota: &AccountQuota, now: i64) -> bool {
    if let Some(weekly_used) = quota.weekly_used {
        let weekly_limit = quota.weekly_limit.unwrap_or(100);
        if weekly_used >= weekly_limit
            && quota
                .weekly_reset_at
                .as_deref()
                .is_none_or(|r| is_future_reset(Some(r), now))
        {
            return true;
        }
    }
    false
}

/// Checks whether any weekly model detail in `quota.model_details` is 100% utilized.
pub fn is_any_weekly_model_detail_exhausted<F>(
    quota: &AccountQuota,
    is_weekly_match: F,
    now: i64,
) -> bool
where
    F: Fn(&str) -> bool,
{
    if let Some(details) = &quota.model_details {
        for detail in details {
            if is_weekly_match(&detail.model_id) {
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
                    return true;
                }
            }
        }
    }
    false
}

/// Checks whether primary session window is ticking with usage and has a future reset timestamp.
pub fn is_session_window_ticking(quota: &AccountQuota, now: i64) -> bool {
    if let Some(session_used) = quota.session_used
        && session_used > 0
        && is_future_reset(quota.session_reset_at.as_deref(), now)
    {
        return true;
    }
    false
}

/// Executes an upstream warmup ping request, logging success/failure and snippet on error.
pub async fn execute_warmup_ping(
    upstream: &Arc<UpstreamClient>,
    req: UpstreamRequest,
    account: &WarmupAccountContext,
    provider: &'static str,
    model: &str,
) -> bool {
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
                provider = %provider,
                account_id = account.account_id,
                model = %model,
                status = %status,
                "[SmartWarmup] Ping failed on {provider} for model '{model}' on account {}: {snippet}",
                account.account_desc,
            );
        }
        Err(e) => {
            tracing::warn!(
                provider = %provider,
                account_id = account.account_id,
                model = %model,
                error = %e,
                "[SmartWarmup] Ping request error on {provider} for model '{model}' on account {}: {e}",
                account.account_desc,
            );
        }
    }

    false
}
