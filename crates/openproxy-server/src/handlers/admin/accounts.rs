use super::*;
use axum::{
    extract::{Path, State, Query},
    Json,
};
use openproxy_core::admin as core_admin;
use openproxy_core::accounts as core_accounts;
use openproxy_core::providers as core_providers;
use openproxy_core::oauth::OAuthProvider;


pub async fn list_accounts(
    State(s): State<AppState>,
    Query(q): Query<AccountListQuery>,
) -> ApiResult<Json<Vec<core_accounts::Account>>> {
    let body: Result<Json<Vec<core_accounts::Account>>, ApiError> = async {
        // Read-only SELECT — use the READER.
        let r = s.db_pool().reader();
        let provider = q.provider_id.map(ProviderId::new);
        let list = core_admin::list_accounts(&r, provider.as_ref())?;
        Ok(Json(list))
    }
    .await;
    body.into()
}

pub async fn create_account(
    State(s): State<AppState>,
    Json(input): Json<core_admin::CreateAccountInput>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        let id = core_admin::create_account(&w, s.master_key().as_ref(), input)?;
        Ok(Json(serde_json::json!({ "id": id.0 })))
    }
    .await;
    body.into()
}

pub async fn delete_account(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        let id = AccountId::new(id);
        core_admin::delete_account(&w, id)?;
        Ok(Json(serde_json::json!({ "deleted": id.0 })))
    }
    .await;
    body.into()
}

pub async fn set_account_health(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let health_str = body
            .get("health")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::Validation("missing 'health' string".into()))?;
        let health = core_accounts::HealthStatus::parse(health_str)?;
        let w = s.db_pool().writer();
        core_accounts::set_health(&w, AccountId::new(id), health)?;
        Ok(Json(serde_json::json!({
            "id": id,
            "health": health_str,
        })))
    }
    .await;
    body.into()
}

pub async fn update_account_api_key(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<core_admin::UpdateAccountApiKeyInput>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        core_admin::update_account_api_key(&w, s.master_key().as_ref(), AccountId::new(id), body)?;
        Ok(Json(serde_json::json!({ "id": id })))
    }
    .await;
    body.into()
}

pub async fn refresh_account_quota(
    State(s): State<AppState>,
    Path(account_id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    tracing::info!(account_id = account_id, "refresh_account_quota: start");
    let s_clone = s.clone();
    let result: Result<Json<serde_json::Value>, ApiError> = async move {
        let account_id = AccountId::new(account_id);

        // 1 + 2 + 3: load the account, gate on provider, decrypt the key.
        // The capability check happens *before* the decrypt so we never
        // touch the master key for a provider whose quota we'll never
        // fetch.
        let (provider_id_str, api_key, access_token, provider_specific) = {
            tracing::debug!(account_id = account_id.0, "refresh_account_quota: acquiring writer");
            let w = s_clone.db_pool().writer();
            tracing::debug!(account_id = account_id.0, "refresh_account_quota: writer acquired");
            let acc = core_admin::account_for_quota_refresh(&w, account_id)?;
            let adapters = s_clone.adapters();
            let supports_quota = adapters
                .iter()
                .find(|a| a.id() == &acc.provider_id)
                .map(|a| a.metadata().quota_refresh_supported)
                .unwrap_or(false);

            if !supports_quota {
                return Ok(Json(serde_json::json!({
                    "account_id": account_id.0,
                    "supported": false,
                    "message": format!(
                        "quota fetching not implemented for provider '{}'",
                        acc.provider_id
                    ),
                })));
            }
            let provider_str = acc.provider_id.to_string();
            let is_oauth = acc.auth_type == "oauth";
            let provider_specific = acc.oauth_provider_specific.clone();

            // OAuth providers (antigravity) need the access token, not
            // an API key. API-key providers need the key. We decrypt
            // whichever is relevant and leave the other empty.
            let (k, token) = if is_oauth {
                let t = core_accounts::decrypt_access_token(
                    &w,
                    account_id,
                    s_clone.master_key().as_ref(),
                )?;
                (String::new(), Some(t))
            } else {
                let k = core_admin::decrypt_api_key_for_account(
                    &w,
                    account_id,
                    s_clone.master_key().as_ref(),
                )?;
                (k, None)
            };
            (provider_str, k, token, provider_specific)
        };
        // writer guard dropped here.

        // 4: fire the upstream quota call. Returns an `AccountQuota`
        //    even on failure (with `fetch_error` set), so we always
        //    have a row to persist.
        let upstream_client = s_clone.upstream_client();
        tracing::info!(account_id = account_id.0, provider = %provider_id_str, "refresh_account_quota: calling upstream");
        let q = core_admin::fetch_account_quota(
            &provider_id_str,
            upstream_client,
            &api_key,
            access_token.as_deref(),
            provider_specific.as_deref(),
        )
        .await;
        tracing::info!(account_id = account_id.0, provider = %provider_id_str, fetch_error = ?q.fetch_error, "refresh_account_quota: upstream done");

        // 4b: If the quota fetch failed with a 401 (expired token) and
        //     we're on an OAuth account, try an on-demand token refresh
        //     and retry the quota call once.
        let q = if q.fetch_error.as_deref().is_some_and(|e| e.contains("401"))
            && access_token.is_some()
        {
            let refresh_result = {
                let w = s_clone.db_pool().writer();
                core_accounts::decrypt_refresh_token(&w, account_id, s_clone.master_key().as_ref())
                    .ok()
                    .flatten()
            };
            if let Some(refresh_token) = refresh_result {
                // Find the matching OAuth provider from the registry.
                let registry = s_clone.oauth_provider_registry();
                let provider = registry.get(&provider_id_str);
                if let Some(provider) = provider {
                    let upstream_client = s_clone.upstream_client();
                    match provider
                        .refresh_token(
                            &refresh_token,
                            upstream_client,
                            account_id,
                            openproxy_core::oauth::DbRef::Pool(s_clone.db_pool().as_ref()),
                        )
                        .await
                    {
                        Ok(new_tokens) => {
                            let expires_at = new_tokens.expires_in.map(|secs| {
                                (chrono::Utc::now() + chrono::Duration::seconds(secs as i64))
                                    .format("%Y-%m-%dT%H:%M:%SZ")
                                    .to_string()
                            });
                            // Store the refreshed tokens.
                            {
                                let w = s_clone.db_pool().writer();
                                let _ = core_accounts::store_oauth_tokens(
                                    &w,
                                    account_id,
                                    &new_tokens.access_token,
                                    new_tokens.refresh_token.as_deref(),
                                    s_clone.master_key(),
                                    &new_tokens.token_type,
                                    expires_at.as_deref(),
                                    new_tokens.scope.as_deref(),
                                    None,
                                    None,
                                );
                            }
                            // Retry the quota call with the new access token.
                            core_admin::fetch_account_quota(
                                &provider_id_str,
                                upstream_client,
                                &api_key,
                                Some(&new_tokens.access_token),
                                provider_specific.as_deref(),
                            )
                            .await
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                account_id = account_id.0,
                                "on-demand token refresh failed"
                            );
                            q // return original error
                        }
                    }
                } else {
                    q
                }
            } else {
                tracing::debug!(
                    account_id = account_id.0,
                    "401 but no refresh token available for on-demand refresh"
                );
                q
            }
        } else {
            q
        };

        // 5: persist.
        {
            let w = s_clone.db_pool().writer();
            core_admin::persist_account_quota(&w, account_id, &q)?;
        }

        // 6: G2.4 — surface a `quota_low` notification when the
        // remaining quota is below the low-water mark. Skipped when
        // the fetch errored (the `quota_*` columns are likely stale
        // or NULL, so a "low quota" reading would be misleading).
        //
        // Threshold: 10% of the limit. If the limit is missing or
        // zero (some providers only report `used`), fall back to an
        // absolute threshold of 1000 — generous enough that a small
        // daily quota account still gets surfaced, tight enough that
        // a "no limit" provider doesn't page on every fetch.
        //
        // Both session and weekly windows are checked; we fire on the
        // FIRST one that crosses the threshold (a single notification
        // per fetch, even if both windows are low). Per-account dedup
        // (`quota_low:{account_id}`) collapses repeats within 24h so
        // the operator isn't paged on every refresh click.
        if q.fetch_error.is_none() {
            let low = compute_low_quota_signal(&q);
            if let Some((scope, remaining, limit)) = low {
                let provider_id_str = provider_id_str.clone();
                let dedup_key = format!(
                    "{}:{}",
                    openproxy_core::notifications::CODE_QUOTA_LOW,
                    account_id.0
                );
                let percent = if limit > 0 {
                    ((remaining as f64) / (limit as f64) * 100.0).round() as u32
                } else {
                    0
                };
                let payload = serde_json::json!({
                    "code": openproxy_core::notifications::CODE_QUOTA_LOW,
                    "message": format!(
                        "Account {} on {} has low {} quota: {} remaining ({}%)",
                        account_id.0, provider_id_str, scope, remaining, percent,
                    ),
                    "provider_id": &provider_id_str,
                    "details": {
                        "account_id": account_id.0,
                        "provider_id": &provider_id_str,
                        "scope": scope,
                        "remaining": remaining,
                        "limit": limit,
                        "percent": percent,
                    },
                });
                let w = s_clone.db_pool().writer();
                let _ = openproxy_core::notifications::insert_and_broadcast(
                    &w,
                    openproxy_core::notifications::KIND_SYSTEM,
                    &payload,
                    Some(&dedup_key),
                    Some(&provider_id_str),
                );
            }
        }

        Ok(Json(serde_json::json!({
            "account_id": account_id.0,
            "supported": true,
            "session_used": q.session_used,
            "session_limit": q.session_limit,
            "session_reset_at": q.session_reset_at,
            "weekly_used": q.weekly_used,
            "weekly_limit": q.weekly_limit,
            "weekly_reset_at": q.weekly_reset_at,
            "plan_name": q.plan_name,
            "last_fetched_at": q.last_fetched_at,
            "error": q.fetch_error,
        })))
    }
    .await;
    result.into()
}

pub(crate) async fn resolve_refresh_account(
    s: &AppState,
    provider: &ProviderId,
    q: &ProviderRefreshQuery,
) -> Result<(Option<AccountId>, String), ApiError> {
    let w = s.db_pool().writer();
    let provider_row = match core_providers::get(&w, provider) {
        Ok(p) => p,
        Err(e) => return Err(ApiError(e)),
    };
    let accounts_list = match core_accounts::list(&w, Some(provider)) {
        Ok(l) => l,
        Err(e) => return Err(ApiError(e)),
    };

    let is_anonymous = match &provider_row {
        Some(p) if matches!(p.auth_type, core_providers::AuthType::None) => true,
        _ if accounts_list.is_empty() => true,
        _ => false,
    };

    if is_anonymous {
        return Ok((None, String::new()));
    }

    let account_id = match q.account_id {
        Some(aid) => Some(AccountId::new(aid)),
        None => accounts_list
            .iter()
            .find(|a| a.health_status == core_accounts::HealthStatus::Healthy)
            .or_else(|| {
                accounts_list
                    .iter()
                    .find(|a| a.health_status == core_accounts::HealthStatus::Degraded)
            })
            .map(|a| a.id),
    };

    if account_id.is_none() {
        let is_anonymous_fallback = provider_row
            .as_ref()
            .map(|p| matches!(p.auth_type, core_providers::AuthType::None))
            .unwrap_or(false);

        if is_anonymous_fallback || accounts_list.is_empty() {
            Ok((None, String::new()))
        } else {
            Err(ApiError(CoreError::NoHealthyTargets(0)))
        }
    } else {
        Ok((account_id, String::new()))
    }
}


pub(crate) fn compute_low_quota_signal(
    q: &openproxy_core::quota::AccountQuota,
) -> Option<(&'static str, i64, i64)> {
    // Session window.
    if let Some(used) = q.session_used
        && let Some(limit) = q.session_limit
    {
        let remaining = (limit - used).max(0);
        if is_low(remaining, limit) {
            return Some(("session", remaining, limit));
        }
    } else if let Some(used) = q.session_used {
        // No limit reported — fall back to the absolute floor on the
        // `used` counter (treat it as a "remaining" proxy: if the
        // account has burned through all but `QUOTA_LOW_ABSOLUTE_FLOOR`
        // of an unknown ceiling, that's still worth surfacing).
        // We can't compute "remaining" without a limit, so this branch
        // only fires when `used` itself is below the floor — i.e. the
        // account is barely touching the upstream. That's not a "low
        // quota" signal, so we intentionally DON'T fire here. Kept as
        // an explicit `else if` to document the reasoning.
        let _ = used;
    }
    // Weekly window.
    if let Some(used) = q.weekly_used
        && let Some(limit) = q.weekly_limit
    {
        let remaining = (limit - used).max(0);
        if is_low(remaining, limit) {
            return Some(("weekly", remaining, limit));
        }
    }
    None
}

pub(crate) fn is_low(remaining: i64, limit: i64) -> bool {
    if limit > 0 {
        // `remaining * 10 < limit` is equivalent to
        // `remaining < limit * 0.10` but stays in integer arithmetic
        // (no float cast, no rounding surprises).
        remaining * 10 < limit
    } else {
        remaining < QUOTA_LOW_ABSOLUTE_FLOOR
    }
}
