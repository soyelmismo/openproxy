use crate::handlers::admin::{AccountId, AppState, ProviderId, core_oauth};

use axum::{Json, extract::Query};
use openproxy_core::accounts as core_accounts;
use openproxy_core::oauth::OAuthProvider;

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
    if account.auth_type.as_ref() != "oauth" {
        return String::new();
    }

    let Some(access_token) = try_decrypt_access_token(s, account.id, provider_id).await else {
        return String::new();
    };

    if !core_oauth::oauth_expires_soon(&account, provider_id.as_str()) {
        return access_token;
    }

    let Some(refresh_token) = try_decrypt_refresh_token(s, account.id, provider_id).await else {
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

    if !provider.supports_refresh() {
        return access_token;
    }

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

async fn try_decrypt_access_token(
    s: &AppState,
    account_id: AccountId,
    provider_id: &ProviderId,
) -> Option<String> {
    let pool = std::sync::Arc::clone(s.db_pool());
    let master_key = std::sync::Arc::clone(s.master_key());
    tokio::task::spawn_blocking(move || {
        let conn = pool.writer();
        core_accounts::decrypt_access_token(&conn, account_id, master_key.as_ref())
    })
    .await
    .ok()
    .and_then(|r| {
        r.map_err(|e| {
            tracing::warn!(
                account = account_id.0,
                provider = %provider_id,
                error = %e,
                "oauth refresh-on-demand: failed to decrypt access token"
            );
        })
        .ok()
    })
}

async fn try_decrypt_refresh_token(
    s: &AppState,
    account_id: AccountId,
    provider_id: &ProviderId,
) -> Option<String> {
    let pool = std::sync::Arc::clone(s.db_pool());
    let master_key = std::sync::Arc::clone(s.master_key());
    let result = tokio::task::spawn_blocking(move || {
        let Some(conn) = pool.try_writer_for(std::time::Duration::from_secs(5)) else {
            return Ok(None);
        };
        core_accounts::decrypt_refresh_token(&conn, account_id, master_key.as_ref())
    })
    .await;
    match result {
        Ok(Ok(Some(t))) => Some(t),
        Ok(Ok(None)) => None,
        Ok(Err(e)) => {
            tracing::warn!(
                account = account_id.0,
                provider = %provider_id,
                error = %e,
                "oauth refresh-on-demand: failed to decrypt refresh token"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                account = account_id.0,
                provider = %provider_id,
                error = %e,
                "oauth refresh-on-demand: spawn_blocking failed"
            );
            None
        }
    }
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
    // Route through the global coordinator so the refresh is serialized per-provider.
    // Providers with rotating refresh tokens (Auth0-backed: MiniMax, Cline, etc.)
    // break when concurrent callers use the same one-time token in parallel.
    let token = match openproxy_core::oauth::TokenRefreshCoordinator::global()
        .refresh_and_store(openproxy_core::oauth::OAuthRefreshParams {
            provider_id: provider_id.as_str(),
            provider: provider.clone(),
            refresh_token,
            upstream_client,
            account_id: account.id,
            db: openproxy_core::oauth::DbRef::Pool(s.db_pool().as_ref()),
            master_key: s.master_key().as_ref(),
            force: true,
        })
        .await
    {
        Ok(t) => {
            tracing::info!(
                account = account.id.0,
                provider = %provider_id,
                "oauth refresh-on-demand: tokens refreshed successfully"
            );
            t
        }
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

    token.access_token
}
