use super::*;
use axum::{
    extract::{Path, State, Query},
    Json,
};
use openproxy_core::admin as core_admin;
use openproxy_core::accounts as core_accounts;
use openproxy_core::providers as core_providers;


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
}pub async fn refresh_account_quota(
    State(s): State<AppState>,
    Path(account_id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    tracing::info!(account_id = account_id, "refresh_account_quota: start");
    let s_clone = s.clone();
    let result: Result<Json<serde_json::Value>, ApiError> = async move {
        let account_id = AccountId::new(account_id);

        let q = openproxy_core::quota_sync::refresh_single_account_quota(
            account_id,
            s_clone.db_pool(),
            s_clone.master_key(),
            &s_clone.adapters(),
            &s_clone.upstream_client(),
            &s_clone.oauth_provider_registry(),
        )
        .await?;

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
