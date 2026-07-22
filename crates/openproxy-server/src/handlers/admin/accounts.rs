use super::*;
use axum::{
    Json,
    extract::{Path, Query, State},
};
use openproxy_core::accounts as core_accounts;
use openproxy_core::admin as core_admin;
use openproxy_core::providers as core_providers;

pub async fn list_accounts(
    State(s): State<AppState>,
    Query(q): Query<AccountListQuery>,
) -> ApiResult<Json<Vec<core_accounts::Account>>> {
    crate::api_try! {
        let provider = q.provider_id.map(ProviderId::new);
        let list = s.services().accounts.list(provider.as_ref(), s.master_key().as_ref())?;
        Ok(Json(list))
    }
}

pub async fn create_account(
    State(s): State<AppState>,
    Json(input): Json<core_admin::CreateAccountInput>,
) -> ApiResult<Json<serde_json::Value>> {
    crate::api_try! {
        let id = s.services().accounts.create(s.master_key().as_ref(), input)?;
        Ok(Json(serde_json::json!({ "id": id.0 })))
    }
}

pub async fn delete_account(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    crate::api_try! {
        let id = AccountId::new(id);
        s.services().accounts.delete(id)?;
        Ok(Json(serde_json::json!({ "deleted": id.0 })))
    }
}

pub async fn set_account_health(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    crate::api_try! {
        let health_str = body
            .get("health")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::Validation("missing 'health' string".into()))?;
        let health = core_accounts::HealthStatus::parse(health_str).map_err(CoreError::Validation)?;
        s.services().accounts.set_health(AccountId::new(id), health)?;
        Ok(Json(serde_json::json!({
            "id": id,
            "health": health_str,
        })))
    }
}

pub async fn update_account_api_key(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<core_admin::UpdateAccountApiKeyInput>,
) -> ApiResult<Json<serde_json::Value>> {
    crate::api_try! {
        s.services().accounts.update_api_key(s.master_key().as_ref(), AccountId::new(id), body)?;
        Ok(Json(serde_json::json!({ "id": id })))
    }
}

pub async fn get_account_api_key(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    crate::api_try! {
        let key = s.services().accounts.get_api_key(s.master_key().as_ref(), AccountId::new(id))?;
        Ok(Json(serde_json::json!({ "api_key": key })))
    }
}

pub async fn update_account_label(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<core_admin::UpdateAccountLabelInput>,
) -> ApiResult<Json<serde_json::Value>> {
    crate::api_try! {
        s.services().accounts.update_label(AccountId::new(id), body)?;
        Ok(Json(serde_json::json!({ "id": id })))
    }
}

pub async fn refresh_account_quota(
    State(s): State<AppState>,
    Path(account_id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    tracing::info!(account_id = account_id, "refresh_account_quota: start");
    let s_clone = s.clone();
    let result: Result<Json<serde_json::Value>, ApiError> = async move {
        let account_id = AccountId::new(account_id);

        let q_opt = openproxy_core::quota_sync::refresh_single_account_quota(
            account_id,
            s_clone.db_pool(),
            s_clone.master_key(),
            &s_clone.adapters(),
            s_clone.upstream_client(),
            &s_clone.oauth_provider_registry(),
        )
        .await?;

        if let Some(q) = q_opt {
            Ok(Json(serde_json::json!({
                "account_id": account_id.0,
                "supported": true,
                "session_used": q.session_used,
                "session_limit": q.session_limit,
                "session_reset_at": q.session_reset_at,
                "weekly_used": q.weekly_used,
                "weekly_limit": q.weekly_limit,
                "weekly_reset_at": q.weekly_reset_at,
                "last_fetched_at": q.last_fetched_at,
                "fetch_error": q.fetch_error,
            })))
        } else {
            Ok(Json(serde_json::json!({
                "account_id": account_id.0,
                "supported": false,
            })))
        }
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
    let accounts_list = match core_accounts::list(&w, Some(provider), s.master_key().as_ref()) {
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

pub async fn apply_account_local_cli(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    crate::api_try! {
        let r = s.db_pool().reader();
        let account_id = AccountId::new(id);

        let account = core_accounts::get(&r, account_id, s.master_key().as_ref())?
            .ok_or_else(|| CoreError::AccountNotFound(account_id.0))?;

        if account.provider_id.as_str() != "antigravity" {
            return Err(CoreError::Validation("Only antigravity accounts can be injected into agy-cli".into()).into());
        }

        let access_token = core_accounts::decrypt_access_token(&r, account_id, s.master_key().as_ref())?;
        let refresh_token = core_accounts::decrypt_refresh_token(&r, account_id, s.master_key().as_ref())?;

        let payload = serde_json::json!({
            "token": {
                "access_token": access_token,
                "token_type": "Bearer",
                "refresh_token": refresh_token.unwrap_or_default(),
                "expiry": account.expires_at.unwrap_or_default(),
            },
            "auth_method": "consumer"
        });

        // Ensure ~/.gemini/antigravity-cli directory exists
        let cli_dir = dirs::home_dir()
            .ok_or_else(|| CoreError::Validation("Could not determine home directory".into()))?
            .join(".gemini")
            .join("antigravity-cli");

        std::fs::create_dir_all(&cli_dir)
            .map_err(|e| CoreError::Validation(format!("Failed to create ~/.gemini/antigravity-cli: {}", e)))?;

        let token_file = cli_dir.join("antigravity-oauth-token");

        std::fs::write(&token_file, serde_json::to_string(&payload).unwrap())
            .map_err(|e| CoreError::Validation(format!("Failed to write to {}: {}", token_file.display(), e)))?;

        Ok(Json(serde_json::json!({
            "success": true,
            "path": token_file.to_string_lossy(),
        })))
    }
}
