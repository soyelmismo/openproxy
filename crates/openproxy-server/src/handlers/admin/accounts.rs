use super::{
    AccountId, ApiError, AppState, CoreError, Deserialize, ProviderId, ProviderRefreshQuery,
};
use crate::extractors::DbReader;
use axum::{
    Json,
    extract::{Path, Query, State},
};
use openproxy_core::accounts as core_accounts;
use openproxy_core::admin as core_admin;
use openproxy_core::providers as core_providers;
use std::io::Write;

/// Query string for `GET /admin/accounts` — supports `?provider_id=...`.
#[derive(Debug, Default, Deserialize)]
pub struct AccountListQuery {
    pub provider_id: Option<String>,
}

pub async fn list_accounts(
    State(s): State<AppState>,
    Query(q): Query<AccountListQuery>,
) -> Result<Json<Vec<core_accounts::Account>>, ApiError> {
    let provider = q.provider_id.map(ProviderId::new);
    let list = s
        .services()
        .accounts
        .list(provider.as_ref(), s.master_key().as_ref())?;
    Ok(Json(list))
}

pub async fn create_account(
    State(s): State<AppState>,
    Json(input): Json<core_admin::CreateAccountInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let provider_id = input.provider_id.clone();
    let id = s
        .services()
        .accounts
        .create(s.master_key().as_ref(), input)?;

    super::providers::spawn_background_provider_refresh(s, provider_id, Some(id.0));

    Ok(Json(serde_json::json!({ "id": id.0 })))
}

crate::admin_entity_action_handler! {
    pub async fn delete_account(
        State(s): State<AppState>,
        Path(id): Path<i64>,
    ) -> Result<Json<serde_json::Value>, ApiError> {
        let id = AccountId::new(id);
        s.services().accounts.delete(id)?;
        Ok(Json(serde_json::json!({ "deleted": id.0 })))
    }
}

pub async fn set_account_health(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let health_str = body
        .get("health")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CoreError::Validation("missing 'health' string".into()))?;
    let health = core_accounts::HealthStatus::parse(health_str).map_err(CoreError::Validation)?;
    s.services()
        .accounts
        .set_health(AccountId::new(id), health)?;
    Ok(Json(serde_json::json!({
        "id": id,
        "health": health_str,
    })))
}

pub async fn update_account_api_key(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<core_admin::UpdateAccountApiKeyInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let acc_id = AccountId::new(id);
    let provider_id = {
        let r = s.db_pool().reader();
        core_accounts::get(&r, acc_id, s.master_key().as_ref())
            .ok()
            .flatten()
            .map(|a| a.provider_id.to_string())
    };
    s.services()
        .accounts
        .update_api_key(s.master_key().as_ref(), acc_id, body)?;

    if let Some(pid) = provider_id {
        super::providers::spawn_background_provider_refresh(s, pid, Some(id));
    }

    Ok(Json(serde_json::json!({ "id": id })))
}

pub async fn get_account_api_key(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let key = s
        .services()
        .accounts
        .get_api_key(s.master_key().as_ref(), AccountId::new(id))?;
    Ok(Json(serde_json::json!({ "api_key": key })))
}

pub async fn update_account_label(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<core_admin::UpdateAccountLabelInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    s.services()
        .accounts
        .update_label(AccountId::new(id), body)?;
    Ok(Json(serde_json::json!({ "id": id })))
}

pub async fn refresh_account_quota(
    State(s): State<AppState>,
    Path(account_id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    tracing::info!(account_id = account_id, "refresh_account_quota: start");
    let result: Result<Json<serde_json::Value>, ApiError> = async move {
        let account_id = AccountId::new(account_id);

        let adapters = s.adapters();
        let supported_providers: Vec<&str> = adapters
            .iter()
            .filter(|a| a.metadata().quota_refresh_supported)
            .map(|a| a.id().as_str())
            .collect();

        let q_opt = openproxy_core::quota_sync::refresh_single_account_quota(
            account_id,
            s.db_pool(),
            s.master_key(),
            &supported_providers,
            s.upstream_client(),
            &s.oauth_provider_registry(),
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
    result
}

pub(crate) fn resolve_refresh_account(
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
            .is_some_and(|p| matches!(p.auth_type, core_providers::AuthType::None));

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
    DbReader(r): DbReader,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let account_id = AccountId::new(id);

    let account = core_accounts::get(&r, account_id, s.master_key().as_ref())?
        .ok_or_else(|| CoreError::AccountNotFound(account_id.0))?;

    if account.provider_id.as_str() != "antigravity" {
        return Err(CoreError::Validation(
            "Only antigravity accounts can be injected into agy-cli".into(),
        )
        .into());
    }

    let access_token =
        core_accounts::decrypt_access_token(&r, account_id, s.master_key().as_ref())?;
    let refresh_token =
        core_accounts::decrypt_refresh_token(&r, account_id, s.master_key().as_ref())?;

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

    std::fs::create_dir_all(&cli_dir).map_err(|e| {
        CoreError::Validation(format!("Failed to create ~/.gemini/antigravity-cli: {e}"))
    })?;

    let token_file = cli_dir.join("antigravity-oauth-token");

    let mut open_options = std::fs::OpenOptions::new();
    open_options.write(true).create(true).truncate(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_options.mode(0o600);
    }

    let mut file = open_options.open(&token_file).map_err(|e| {
        CoreError::Validation(format!("Failed to open {}: {}", token_file.display(), e))
    })?;

    let payload_str = serde_json::to_string(&payload)
        .map_err(|e| CoreError::Validation(format!("Failed to serialize payload: {e}")))?;
    file.write_all(payload_str.as_bytes()).map_err(|e| {
        CoreError::Validation(format!(
            "Failed to write to {}: {}",
            token_file.display(),
            e
        ))
    })?;

    Ok(Json(serde_json::json!({
        "success": true,
        "path": token_file.to_string_lossy(),
    })))
}
