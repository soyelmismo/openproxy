//! Helpers extracted from `AntigravityOAuthProvider::post_exchange`.
//!
//! Each helper owns one of the three logical blocks of the
//! post-exchange pipeline: fetch user email, bootstrap the Cloud
//! Code `projectId` via `loadCodeAssist` + `onboardUser`, and
//! persist the resulting metadata + email on the account row.
//!
//! Splitting them keeps the trait method in `mod.rs` as a thin
//! orchestrator and lets each block be reviewed independently.

use std::sync::Arc;

use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use openproxy_db::DbPool;

use crate::error::{CoreError, Result};
use crate::ids::AccountId;

use super::AntigravityProviderMeta;

/// Fetch the user's email from the Google `userinfo` endpoint.
///
/// Best-effort: returns `None` on any failure (network, non-2xx,
/// malformed JSON, missing `email` field). Callers must treat the
/// `None` branch as "we don't know the email" and continue without
/// raising an error.
pub(crate) async fn fetch_user_email(
    upstream: &Arc<UpstreamClient>,
    access_token: &str,
) -> Option<String> {
    let user_info_url = "https://www.googleapis.com/oauth2/v1/userinfo?alt=json";
    let mut req = UpstreamRequest::get(user_info_url);
    // Surface invalid bearer tokens as a soft failure: this helper is
    // best-effort and any header issue must skip the call without
    // sending a malformed Authorization header upstream.
    if let Err(e) = openproxy_adapters::antigravity_headers::insert_bearer(&mut req, access_token) {
        tracing::debug!(
            access_token_len = access_token.len(),
            error = %e,
            "antigravity post_exchange: skipping userinfo fetch due to invalid bearer"
        );
        return None;
    }
    req.is_streaming = false;
    let cancel = CancellationToken::new();
    match upstream.call(req, TimeoutProfile::OAuth, cancel).await {
        Ok(resp) if resp.status.is_success() => {
            let body = resp.collect().await.unwrap_or_default();
            serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("email").and_then(|e| e.as_str()).map(String::from))
        }
        _ => None,
    }
}

/// Bootstrap the Cloud Code `projectId` for this account.
///
/// Calls `loadCodeAssist` first. If the user is already on-boarded
/// the response carries a `projectId` and we're done. Otherwise we
/// enter the `onboardUser` retry loop: up to 15 attempts with
/// exponential backoff (50ms → 100ms → ... capped at 2s) until
/// `onboardUser` returns a `projectId`, errors out, or the loop is
/// exhausted (in which case we surface an `Internal` error).
pub(crate) async fn bootstrap_project_id(
    upstream: &Arc<UpstreamClient>,
    access_token: &str,
) -> Result<String> {
    let metadata = serde_json::json!({
        "ideType": "ANTIGRAVITY",
    });

    match openproxy_adapters::adapters::antigravity::load_code_assist(
        upstream,
        access_token,
        &metadata,
    )
    .await
    .map_err(CoreError::UpstreamConnection)?
    {
        Some(pid) => Ok(pid),
        None => {
            // Retry onboardUser up to 15 times with exponential backoff
            let mut result = None;
            let mut delay = std::time::Duration::from_millis(50);
            for attempt in 0..15 {
                match openproxy_adapters::adapters::antigravity::onboard_user(
                    upstream,
                    access_token,
                    "",
                    &metadata,
                )
                .await
                {
                    Ok(Some(pid)) => {
                        result = Some(pid);
                        break;
                    }
                    Ok(None) => {
                        // Not done yet, wait and retry
                        tokio::time::sleep(delay).await;
                        delay = std::cmp::min(delay * 2, std::time::Duration::from_secs(2));
                    }
                    Err(e) => {
                        tracing::warn!(attempt = attempt + 1, error = %e, "onboardUser failed");
                        break;
                    }
                }
            }
            match result {
                Some(pid) => Ok(pid),
                None => {
                    tracing::warn!("onboardUser did not complete after 15 attempts");
                    Err(CoreError::Internal(
                        "onboardUser did not complete after 15 attempts".into(),
                    ))
                }
            }
        }
    }
}

/// Persist `project_id` (and optionally `email`) on the account row.
///
/// Two UPDATEs:
///
/// 1. Always: serialize `AntigravityProviderMeta { project_id }` into
///    `accounts.oauth_provider_specific`. The chat executor reads this
///    JSON envelope to embed `projectId` in upstream requests.
/// 2. If `email` is `Some`: set `accounts.email` and backfill
///    `accounts.label` when it's currently empty (the COALESCE/NULLIF
///    combination preserves any user-supplied label).
///
/// Each `UPDATE` acquires the writer connection briefly inside a
/// scoped block so the guard is released before any subsequent
/// `.await`.
pub(crate) async fn persist_post_exchange_meta(
    db_pool: &Arc<DbPool>,
    account_id: AccountId,
    project_id: String,
    email: Option<String>,
) -> Result<()> {
    // 1. Always write the projectId envelope.
    let meta = AntigravityProviderMeta {
        project_id: Some(project_id),
    };
    let meta_json = serde_json::to_string(&meta)
        .map_err(|e| CoreError::Internal(format!("antigravity meta serialize: {e}")))?;
    {
        let conn = db_pool.writer();
        conn.execute(
            "UPDATE accounts SET oauth_provider_specific = ?1 WHERE id = ?2",
            rusqlite::params![meta_json, account_id.0],
        )
        .map_err(openproxy_db::error::map_db_error_ctx(format!(
            "antigravity post_exchange update project_id for account {}",
            account_id.0
        )))?;
    }

    // 2. Best-effort email + label backfill.
    if let Some(ref email) = email {
        let conn = db_pool.writer();
        conn.execute(
            "UPDATE accounts SET email = ?1, label = COALESCE(NULLIF(label, ''), ?1) WHERE id = ?2",
            rusqlite::params![email, account_id.0],
        )
        .map_err(openproxy_db::error::map_db_error_ctx(format!(
            "antigravity post_exchange update email and label for account {}",
            account_id.0
        )))?;
    }

    Ok(())
}
