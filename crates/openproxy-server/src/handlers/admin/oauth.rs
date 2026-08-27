use super::{AccountId, ApiError, AppState, CoreError, ProviderId, core_oauth};
use axum::{
    Json,
    extract::{Path, Query, State},
};

use openproxy_core::accounts as core_accounts;
use openproxy_core::oauth::{OAuthProvider, OAuthProviderEnum, TokenResponse};

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/{provider}/authorize", axum::routing::get(oauth_authorize))
        .route("/{provider}/exchange", axum::routing::post(oauth_exchange))
        .route(
            "/{provider}/device-code",
            axum::routing::post(oauth_device_code),
        )
        .route(
            "/{provider}/device-poll",
            axum::routing::post(oauth_device_poll),
        )
}

fn validate_authorize_flow(
    provider: &str,
    provider_impl: &OAuthProviderEnum,
) -> Result<(), ApiError> {
    let flow = provider_impl.flow();
    if flow != openproxy_core::oauth::OAuthFlow::AuthorizationCodePkce
        && flow != openproxy_core::oauth::OAuthFlow::AuthorizationCode
    {
        return Err(ApiError(CoreError::Validation(format!(
            "provider '{provider}' does not support authorization code flow"
        ))));
    }
    Ok(())
}

fn get_oauth_redirect_uri() -> String {
    let web_port = std::env::var("OPENPROXY_WEB_PORT").unwrap_or_else(|_| "8788".to_string());
    format!("http://localhost:{web_port}/admin/callback.html")
}

pub async fn oauth_authorize(
    State(s): State<AppState>,
    Path(provider): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let registry = s.oauth_provider_registry();
    let provider_impl = registry.get(&provider).ok_or_else(|| {
        ApiError(CoreError::Validation(format!(
            "provider '{provider}' does not support OAuth authorize"
        )))
    })?;

    validate_authorize_flow(&provider, &provider_impl)?;
    let redirect_uri = get_oauth_redirect_uri();
    let (auth_url, code_verifier, _, state) = provider_impl.build_auth_url(&redirect_uri).await?;

    Ok(Json(serde_json::json!({
        "authorization_url": auth_url,
        "code_verifier": code_verifier,
        "redirect_uri": redirect_uri,
        "state": state,
    })))
}

fn resolve_or_create_oauth_account(
    s: &AppState,
    provider: &str,
    account_id_input: Option<i64>,
) -> Result<AccountId, ApiError> {
    if let Some(id) = account_id_input {
        return Ok(AccountId(id));
    }
    tokio::task::block_in_place(|| {
        let w = s.db_pool().writer();
        let provider_id = ProviderId::new(provider);
        core_accounts::create(&w, &provider_id, None, s.master_key(), None, 10, None)
    })
    .map_err(ApiError)
}

fn compute_oauth_expires_at(expires_in: Option<u64>) -> Option<String> {
    expires_in.map(|secs| {
        (chrono::Utc::now() + chrono::Duration::seconds(secs as i64))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    })
}

async fn save_oauth_token_and_notify(
    s: &AppState,
    provider: &str,
    provider_impl: &OAuthProviderEnum,
    account_id: AccountId,
    token: &TokenResponse,
    custom_provider_specific: Option<String>,
) -> Result<(), ApiError> {
    let expires_at = compute_oauth_expires_at(token.expires_in);
    let provider_specific =
        custom_provider_specific.or_else(|| provider_impl.provider_specific_from_token(token));
    let email = provider_impl.email_from_token(token);

    tokio::task::block_in_place(|| {
        let w = s.db_pool().writer();
        openproxy_core::accounts::store_oauth_tokens(
            &w,
            account_id,
            s.master_key(),
            openproxy_core::accounts::StoreOAuthTokensParams {
                access_token: &token.access_token,
                refresh_token: token.refresh_token.as_deref(),
                token_type: &token.token_type,
                expires_at: expires_at.as_deref(),
                scope: token.scope.as_deref(),
                provider_specific: provider_specific.as_deref(),
                email: email.as_deref(),
            },
        )
    })?;

    if let Err(e) = provider_impl
        .post_exchange(account_id, s.db_pool(), s.master_key(), s.upstream_client())
        .await
    {
        tracing::warn!(
            provider = %provider,
            account_id = account_id.0,
            error = %e,
            "post-exchange hook failed",
        );
    }

    super::providers::spawn_background_provider_refresh(
        s.clone(),
        provider.to_string(),
        Some(account_id.0),
    );

    Ok(())
}

pub async fn oauth_exchange(
    State(s): State<AppState>,
    Path(provider): Path<String>,
    Json(input): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let code = input
        .get("code")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CoreError::Validation("missing 'code'".into()))?;
    let code_verifier = input
        .get("code_verifier")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let account_id_input = input.get("account_id").and_then(serde_json::Value::as_i64);
    let redirect_uri = input
        .get("redirect_uri")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CoreError::Validation("missing 'redirect_uri'".into()))?;

    let registry = s.oauth_provider_registry();
    let provider_impl = registry.get(&provider).ok_or_else(|| {
        ApiError(CoreError::Validation(format!(
            "provider '{provider}' does not support OAuth exchange"
        )))
    })?;

    let token = provider_impl
        .exchange_code(code, code_verifier, s.upstream_client(), redirect_uri)
        .await?;

    let account_id = resolve_or_create_oauth_account(&s, &provider, account_id_input)?;
    save_oauth_token_and_notify(&s, &provider, &provider_impl, account_id, &token, None).await?;

    Ok(Json(serde_json::json!({
        "account_id": account_id.0,
        "provider": provider,
        "status": "connected",
    })))
}

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

    tokio::task::block_in_place(|| {
        let w = s.db_pool().writer();
        openproxy_core::oauth::tickets::create_ticket(&w, &provider, &dar)
    })?;

    Ok(Json(serde_json::json!({
        "device_code": dar.device_code,
        "user_code": dar.user_code,
        "verification_uri": dar.verification_uri,
        "verification_uri_complete": dar.verification_uri_complete,
        "expires_in": dar.expires_in,
        "interval": dar.interval,
    })))
}

fn validate_active_ticket(s: &AppState, device_code: &str) -> Result<(), ApiError> {
    tokio::task::block_in_place(|| {
        let w = s.db_pool().writer();
        match openproxy_core::oauth::tickets::lookup_active(&w, device_code)? {
            openproxy_core::oauth::tickets::TicketStatus::Active(_) => Ok(()),
            openproxy_core::oauth::tickets::TicketStatus::Expired => Err(ApiError(
                CoreError::Validation("device_code has expired; restart the OAuth flow".into()),
            )),
            openproxy_core::oauth::tickets::TicketStatus::Consumed
            | openproxy_core::oauth::tickets::TicketStatus::Unknown => {
                Err(ApiError(CoreError::NotFound {
                    what: "oauth_device_ticket".into(),
                    id: device_code.to_string(),
                }))
            }
        }
    })
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

    validate_active_ticket(&s, device_code)?;

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

    let account_id = resolve_or_create_oauth_account(&s, &provider, account_id_input)?;
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

    tokio::task::block_in_place(|| {
        let w = s.db_pool().writer();
        let _ = openproxy_core::oauth::tickets::mark_consumed(&w, device_code);
    });

    Ok(Json(serde_json::json!({
        "status": "ok",
        "account_id": account_id.0,
    })))
}

pub async fn oauth_callback(
    Query(mut params): Query<std::collections::BTreeMap<String, String>>,
) -> Json<serde_json::Value> {
    let code = params.remove("code").unwrap_or_default();
    let state = params.remove("state");
    // Sanitize: never return raw error details from upstream providers —
    // they may contain URLs with tokens, internal error codes, or
    // other sensitive information. Map known error types to generic
    // messages and drop anything else.
    let error = params.get("error").map(|raw| match raw.as_str() {
        "access_denied" => "access_denied",
        "server_error" => "server_error",
        "temporarily_unavailable" => "temporarily_unavailable",
        _ => "authorization_failed",
    });

    Json(serde_json::json!({
        "code": if code.is_empty() { None::<String> } else { Some(code) },
        "error": error.map(String::from),
        "state": state,
        "message": "Copy the code above and paste it into the Exchange endpoint.",
    }))
}

pub(crate) async fn refresh_oauth_if_needed(
    s: &AppState,
    account: core_accounts::Account,
    provider_id: &ProviderId,
) -> String {
    if account.auth_type != "oauth" {
        return String::new();
    }

    let Some(access_token) = try_decrypt_access_token(s, account.id, provider_id) else {
        return String::new();
    };

    if !core_oauth::oauth_expires_soon(&account, provider_id.as_str()) {
        return access_token;
    }

    let Some(refresh_token) = try_decrypt_refresh_token(s, account.id, provider_id) else {
        return access_token;
    };

    let registry = s.oauth_provider_registry();
    let Some(provider) = registry.get(provider_id.as_str()) else {
        tracing::warn!(
            account = account.id.0,
            provider = %provider_id,
            "oauth refresh-on-demand: no provider impl found"
        );
        return access_token;
    };

    tracing::info!(
        account = account.id.0,
        provider = %provider_id,
        "oauth refresh-on-demand: refreshing expired/expiring token"
    );

    execute_oauth_refresh(
        s,
        &account,
        provider_id,
        &refresh_token,
        &provider,
        access_token,
    )
    .await
}

fn try_decrypt_access_token(
    s: &AppState,
    account_id: AccountId,
    provider_id: &ProviderId,
) -> Option<String> {
    let conn = s.db_pool().writer();
    core_accounts::decrypt_access_token(&conn, account_id, s.master_key().as_ref())
        .map_err(|e| {
            tracing::warn!(
                account = account_id.0,
                provider = %provider_id,
                error = %e,
                "oauth refresh-on-demand: failed to decrypt access token"
            );
        })
        .ok()
}

fn try_decrypt_refresh_token(
    s: &AppState,
    account_id: AccountId,
    provider_id: &ProviderId,
) -> Option<String> {
    let conn = s.db_pool().writer();
    core_accounts::decrypt_refresh_token(&conn, account_id, s.master_key().as_ref())
        .map_err(|e| {
            tracing::warn!(
                account = account_id.0,
                provider = %provider_id,
                error = %e,
                "oauth refresh-on-demand: failed to decrypt refresh token"
            );
        })
        .ok()
        .flatten()
}

async fn execute_oauth_refresh(
    s: &AppState,
    account: &core_accounts::Account,
    provider_id: &ProviderId,
    refresh_token: &str,
    provider: &core_oauth::OAuthProviderEnum,
    fallback_token: String,
) -> String {
    let upstream_client = s.upstream_client();
    let token = match provider
        .refresh_token(
            refresh_token,
            upstream_client,
            account.id,
            openproxy_core::oauth::DbRef::Pool(s.db_pool().as_ref()),
        )
        .await
    {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(
                account = account.id.0,
                provider = %provider_id,
                error = %e,
                "oauth refresh-on-demand: token refresh failed"
            );
            return fallback_token;
        }
    };

    let expires_at = token.expires_in.map(|secs| {
        (chrono::Utc::now() + chrono::Duration::seconds(secs as i64))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    });

    let conn = s.db_pool().writer();
    match core_accounts::store_oauth_tokens(
        &conn,
        account.id,
        s.master_key().as_ref(),
        core_accounts::StoreOAuthTokensParams {
            access_token: &token.access_token,
            refresh_token: token.refresh_token.as_deref(),
            token_type: &token.token_type,
            expires_at: expires_at.as_deref(),
            scope: token.scope.as_deref(),
            provider_specific: account.oauth_provider_specific.as_deref(),
            email: account.email.as_deref(),
        },
    ) {
        Ok(()) => {
            tracing::info!(
                account = account.id.0,
                provider = %provider_id,
                "oauth refresh-on-demand: tokens refreshed successfully"
            );
            token.access_token
        }
        Err(e) => {
            tracing::warn!(
                account = account.id.0,
                provider = %provider_id,
                error = %e,
                "oauth refresh-on-demand: failed to store refreshed tokens"
            );
            fallback_token
        }
    }
}
