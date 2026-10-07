//! Manual Codex rate limit reset credit operations.

use openproxy_adapters::CancellationToken;
use openproxy_adapters::adapters::codex::quota::{
    CodexResetCredit, CodexResetOutcome, build_codex_consume_reset_request,
    build_codex_reset_credits_request, parse_codex_consume_response, parse_codex_reset_credits,
};
use openproxy_adapters::upstream::{TimeoutProfile, UpstreamClient};
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use openproxy_types::ids::AccountId;
use openproxy_types::{CoreError, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::accounts;
use crate::admin;
use crate::oauth::{DbRef, OAuthProviderRegistry, OAuthRefreshParams, TokenRefreshCoordinator};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexResetResult {
    pub success: bool,
    pub outcome: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexResetListResponse {
    pub available_count: u32,
    pub credits: Vec<CodexResetCredit>,
}

struct AccountCodexContext {
    access_token: String,
    refresh_token: Option<String>,
    provider_specific: Option<Box<str>>,
}

async fn load_codex_context(
    account_id: AccountId,
    db_pool: &Arc<DbPool>,
    master_key: &Arc<MasterKey>,
) -> Result<AccountCodexContext> {
    let pool = Arc::clone(db_pool);
    let key = Arc::clone(master_key);

    tokio::task::spawn_blocking(move || {
        let r = pool.reader();
        let acc = admin::account_for_quota_refresh(&r, account_id, key.as_ref())?;

        if acc.provider_id.as_str() != "codex" {
            return Err(CoreError::Validation(
                "Codex reset credits can only be redeemed for Codex accounts".into(),
            ));
        }

        if acc.auth_type.as_ref() != "oauth" {
            return Err(CoreError::Validation(
                "Codex reset credits require an OAuth account".into(),
            ));
        }

        let access_token = accounts::decrypt_access_token(&r, account_id, &key)?;
        let refresh_token = accounts::decrypt_refresh_token(&r, account_id, &key)
            .ok()
            .flatten();

        Ok(AccountCodexContext {
            access_token,
            refresh_token,
            provider_specific: acc.oauth_provider_specific,
        })
    })
    .await
    .map_err(|e| CoreError::Internal(e.to_string()))?
}

async fn try_refresh_codex_token(
    account_id: AccountId,
    refresh_token: &str,
    db_pool: &Arc<DbPool>,
    master_key: &Arc<MasterKey>,
    upstream_client: &Arc<UpstreamClient>,
    oauth_registry: &Arc<OAuthProviderRegistry>,
) -> Result<String> {
    let Some(provider) = oauth_registry.get("codex") else {
        return Err(CoreError::Internal(
            "codex oauth provider not registered".into(),
        ));
    };

    TokenRefreshCoordinator::global()
        .refresh_and_store(OAuthRefreshParams {
            provider_id: "codex",
            provider,
            refresh_token,
            upstream_client,
            account_id,
            db: DbRef::Pool(db_pool.as_ref()),
            master_key: master_key.as_ref(),
            force: true,
        })
        .await?;

    let pool = Arc::clone(db_pool);
    let key = Arc::clone(master_key);
    tokio::task::spawn_blocking(move || {
        let r = pool.reader();
        accounts::decrypt_access_token(&r, account_id, &key)
    })
    .await
    .map_err(|e| CoreError::Internal(e.to_string()))?
}

pub async fn list_codex_account_resets(
    account_id: AccountId,
    db_pool: &Arc<DbPool>,
    master_key: &Arc<MasterKey>,
    upstream_client: &Arc<UpstreamClient>,
    oauth_registry: &Arc<OAuthProviderRegistry>,
) -> Result<CodexResetListResponse> {
    let mut ctx = load_codex_context(account_id, db_pool, master_key).await?;

    let req =
        build_codex_reset_credits_request(&ctx.access_token, ctx.provider_specific.as_deref());
    let cancel = CancellationToken::new();
    let mut resp = upstream_client
        .call(req, TimeoutProfile::Chat, cancel.clone())
        .await
        .map_err(|e| CoreError::UpstreamConnection(e.to_string()))?;

    if (resp.status.as_u16() == 401 || resp.status.as_u16() == 403)
        && let Some(ref rt) = ctx.refresh_token
        && let Ok(new_token) = try_refresh_codex_token(
            account_id,
            rt,
            db_pool,
            master_key,
            upstream_client,
            oauth_registry,
        )
        .await
    {
        ctx.access_token = new_token;
        let retry_req =
            build_codex_reset_credits_request(&ctx.access_token, ctx.provider_specific.as_deref());
        if let Ok(retry_resp) = upstream_client
            .call(retry_req, TimeoutProfile::Chat, cancel)
            .await
        {
            resp = retry_resp;
        }
    }

    let status = resp.status.as_u16();
    let body = resp
        .collect()
        .await
        .map_err(|e| CoreError::UpstreamConnection(e.to_string()))?;
    let json: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| CoreError::Parse(format!("codex reset credits parse: {e}")))?;

    if !(200..300).contains(&status) {
        return Err(CoreError::upstream_error(
            status,
            "codex",
            "codex-reset-list",
            json.to_string(),
            false,
        ));
    }

    let (credits, available_count) = parse_codex_reset_credits(&json)?;
    Ok(CodexResetListResponse {
        available_count,
        credits,
    })
}

pub async fn redeem_codex_account_reset(
    account_id: AccountId,
    target_credit_id: Option<&str>,
    db_pool: &Arc<DbPool>,
    master_key: &Arc<MasterKey>,
    upstream_client: &Arc<UpstreamClient>,
    oauth_registry: &Arc<OAuthProviderRegistry>,
) -> Result<CodexResetResult> {
    let mut ctx = load_codex_context(account_id, db_pool, master_key).await?;

    let req_list =
        build_codex_reset_credits_request(&ctx.access_token, ctx.provider_specific.as_deref());
    let cancel = CancellationToken::new();
    let mut resp = upstream_client
        .call(req_list, TimeoutProfile::Chat, cancel.clone())
        .await
        .map_err(|e| CoreError::UpstreamConnection(e.to_string()))?;

    if (resp.status.as_u16() == 401 || resp.status.as_u16() == 403)
        && let Some(ref rt) = ctx.refresh_token
        && let Ok(new_token) = try_refresh_codex_token(
            account_id,
            rt,
            db_pool,
            master_key,
            upstream_client,
            oauth_registry,
        )
        .await
    {
        ctx.access_token = new_token;
        let retry_req =
            build_codex_reset_credits_request(&ctx.access_token, ctx.provider_specific.as_deref());
        if let Ok(retry_resp) = upstream_client
            .call(retry_req, TimeoutProfile::Chat, cancel.clone())
            .await
        {
            resp = retry_resp;
        }
    }

    let status = resp.status.as_u16();
    let body = resp
        .collect()
        .await
        .map_err(|e| CoreError::UpstreamConnection(e.to_string()))?;
    let json: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| CoreError::Parse(format!("codex reset credits parse: {e}")))?;

    if !(200..300).contains(&status) {
        return Err(CoreError::upstream_error(
            status,
            "codex",
            "codex-reset-list",
            json.to_string(),
            false,
        ));
    }

    let (credits, _) = parse_codex_reset_credits(&json)?;
    let target_credit = if let Some(target_id) = target_credit_id.filter(|s| !s.trim().is_empty()) {
        credits
            .into_iter()
            .find(|c| c.id == target_id)
            .unwrap_or_else(|| CodexResetCredit {
                id: target_id.to_string(),
                reset_type: None,
                status: None,
                expires_at: None,
                title: None,
                description: None,
            })
    } else {
        let Some(first) = credits.into_iter().next() else {
            return Ok(CodexResetResult {
                success: false,
                outcome: CodexResetOutcome::NoCredit.as_str().into(),
                message: "No Codex reset credits are available for this account.".into(),
            });
        };
        first
    };

    let idempotency_key = uuid::Uuid::new_v4().to_string();
    let consume_req = build_codex_consume_reset_request(
        &ctx.access_token,
        ctx.provider_specific.as_deref(),
        &idempotency_key,
        &target_credit.id,
    );

    let mut consume_resp = upstream_client
        .call(consume_req, TimeoutProfile::Chat, cancel.clone())
        .await
        .map_err(|e| CoreError::UpstreamConnection(e.to_string()))?;

    if (consume_resp.status.as_u16() == 401 || consume_resp.status.as_u16() == 403)
        && let Some(ref rt) = ctx.refresh_token
        && let Ok(new_token) = try_refresh_codex_token(
            account_id,
            rt,
            db_pool,
            master_key,
            upstream_client,
            oauth_registry,
        )
        .await
    {
        ctx.access_token = new_token;
        let retry_consume_req = build_codex_consume_reset_request(
            &ctx.access_token,
            ctx.provider_specific.as_deref(),
            &idempotency_key,
            &target_credit.id,
        );
        if let Ok(retry_resp) = upstream_client
            .call(retry_consume_req, TimeoutProfile::Chat, cancel)
            .await
        {
            consume_resp = retry_resp;
        }
    }

    let consume_status = consume_resp.status.as_u16();
    let consume_body = consume_resp
        .collect()
        .await
        .map_err(|e| CoreError::UpstreamConnection(e.to_string()))?;
    let consume_json: serde_json::Value =
        serde_json::from_slice(&consume_body).unwrap_or_else(|_| {
            serde_json::Value::String(String::from_utf8_lossy(&consume_body).into_owned())
        });

    let outcome = parse_codex_consume_response(consume_status, &consume_json)?;

    let (success, message) = match outcome {
        CodexResetOutcome::Reset => {
            // Trigger automatic quota refresh so UI and DB get the fresh limit immediately!
            let _ = crate::quota_sync::refresh_single_account_quota(
                account_id,
                db_pool,
                master_key,
                &["codex"],
                upstream_client,
                oauth_registry,
            )
            .await;
            (
                true,
                "Codex rate limit reset credit applied successfully.".to_string(),
            )
        }
        CodexResetOutcome::AlreadyRedeemed => {
            let _ = crate::quota_sync::refresh_single_account_quota(
                account_id,
                db_pool,
                master_key,
                &["codex"],
                upstream_client,
                oauth_registry,
            )
            .await;
            (
                true,
                "Codex rate limit reset credit was already redeemed.".to_string(),
            )
        }
        CodexResetOutcome::NothingToReset => (
            false,
            "No exhausted Codex usage limit can be reset right now.".to_string(),
        ),
        CodexResetOutcome::NoCredit => (false, "No Codex reset credits are available.".to_string()),
    };

    Ok(CodexResetResult {
        success,
        outcome: outcome.as_str().to_string(),
        message,
    })
}
