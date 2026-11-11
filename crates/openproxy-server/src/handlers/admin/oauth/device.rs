use crate::handlers::admin::{ApiError, AppState, CoreError};

use axum::{
    Json,
    extract::{Path, State},
};
use openproxy_core::oauth::{OAuthProvider, OAuthProviderEnum, TokenResponse};

use super::{resolve_or_create_oauth_account, save_oauth_token_and_notify};

pub async fn oauth_device_code(
    State(s): State<AppState>,
    Path(provider): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let registry = s.oauth_provider_registry();
    let provider_impl = registry.get(&provider).ok_or_else(|| {
        ApiError(CoreError::Validation(format!(
            "provider '{provider}' does not support device code authorization"
        )))
    })?;

    let dar = provider_impl
        .request_device_code(s.upstream_client())
        .await?;

    let pool = std::sync::Arc::clone(s.db_pool());
    let provider_clone = provider.clone();
    let dar_clone = dar.clone();
    tokio::task::spawn_blocking(move || -> Result<(), CoreError> {
        let w = pool
            .try_writer_for(openproxy_db::conn::ADMIN_LOCK_TIMEOUT)
            .ok_or_else(|| CoreError::Internal("writer lock timeout".into()))?;
        openproxy_core::oauth::tickets::create_ticket(&w, &provider_clone, &dar_clone)?;
        Ok(())
    })
    .await
    .map_err(|e| ApiError(CoreError::Internal(format!("spawn failed: {e}"))))??;

    Ok(Json(serde_json::json!({
        "device_code": dar.device_code,
        "user_code": dar.user_code,
        "verification_uri": dar.verification_uri,
        "verification_uri_complete": dar.verification_uri_complete,
        "expires_in": dar.expires_in,
        "interval": dar.interval,
    })))
}

async fn validate_active_ticket(s: &AppState, device_code: &str) -> Result<(), ApiError> {
    let pool = std::sync::Arc::clone(s.db_pool());
    let device_code = device_code.to_string();
    tokio::task::spawn_blocking(move || -> Result<(), ApiError> {
        let r = pool
            .try_reader_for(std::time::Duration::from_secs(5))
            .ok_or_else(|| ApiError(CoreError::Internal("reader lock timeout".into())))?;
        match openproxy_core::oauth::tickets::lookup_active(&r, &device_code)? {
            openproxy_core::oauth::tickets::TicketStatus::Active(_) => Ok(()),
            openproxy_core::oauth::tickets::TicketStatus::Expired => Err(ApiError(
                CoreError::Validation("device_code has expired; restart the OAuth flow".into()),
            )),
            openproxy_core::oauth::tickets::TicketStatus::Consumed
            | openproxy_core::oauth::tickets::TicketStatus::Unknown => Err(ApiError(
                CoreError::not_found("oauth_device_ticket", &device_code),
            )),
        }
    })
    .await
    .map_err(|e| ApiError(CoreError::Internal(format!("spawn failed: {e}"))))?
}

fn resolve_kiro_or_default_provider_specific(
    provider: &str,
    provider_impl: &OAuthProviderEnum,
    token: &TokenResponse,
) -> Option<String> {
    if provider == "kiro" {
        openproxy_core::oauth::kiro::take_last_client().map(|(cid, csec)| {
            serde_json::json!({
                "client_id": cid,
                "client_secret": csec,
                "region": openproxy_core::oauth::kiro::KiroProviderMeta::default().region,
            })
            .to_string()
        })
    } else {
        provider_impl.provider_specific_from_token(token)
    }
}

pub async fn oauth_device_poll(
    State(s): State<AppState>,
    Path(provider): Path<String>,
    Json(input): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let device_code = input
        .get("device_code")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CoreError::Validation("missing 'device_code'".into()))?;
    let account_id_input = input.get("account_id").and_then(serde_json::Value::as_i64);

    validate_active_ticket(&s, device_code).await?;

    let registry = s.oauth_provider_registry();
    let provider_impl = registry.get(&provider).ok_or_else(|| {
        ApiError(CoreError::Validation(format!(
            "provider '{provider}' does not support device code polling"
        )))
    })?;

    let Some(token) = provider_impl
        .poll_device_token(device_code, s.upstream_client())
        .await?
    else {
        return Ok(Json(serde_json::json!({ "status": "pending" })));
    };

    let account_id = resolve_or_create_oauth_account(&s, &provider, account_id_input).await?;
    let provider_specific =
        resolve_kiro_or_default_provider_specific(&provider, &provider_impl, &token);

    save_oauth_token_and_notify(
        &s,
        &provider,
        &provider_impl,
        account_id,
        &token,
        provider_specific,
    )
    .await?;

    let pool = std::sync::Arc::clone(s.db_pool());
    let device_code_clone = device_code.to_string();
    tokio::task::spawn_blocking(move || {
        if let Some(w) = pool.try_writer_for(openproxy_db::conn::ADMIN_LOCK_TIMEOUT) {
            let _ = openproxy_core::oauth::tickets::mark_consumed(&w, &device_code_clone);
        }
    })
    .await
    .map_err(|e| ApiError(CoreError::Internal(format!("spawn failed: {e}"))))?;

    Ok(Json(serde_json::json!({
        "status": "ok",
        "account_id": account_id.0,
    })))
}
