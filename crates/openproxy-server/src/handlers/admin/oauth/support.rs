use crate::handlers::admin::{AccountId, ApiError, AppState, CoreError, ProviderId};

use openproxy_core::accounts as core_accounts;
use openproxy_core::oauth::{OAuthProvider, OAuthProviderEnum, TokenResponse};

pub(crate) fn get_oauth_redirect_uri(s: &AppState) -> String {
    if let Ok(web_port) = std::env::var("OPENPROXY_WEB_PORT") {
        return format!("http://localhost:{web_port}/admin/callback.html");
    }
    let port = s
        .config()
        .server
        .bind
        .rsplit_once(':')
        .map_or("8787", |(_, p)| p);
    format!("http://localhost:{port}/admin/callback.html")
}

pub(crate) async fn resolve_or_create_oauth_account(
    s: &AppState,
    provider: &str,
    account_id_input: Option<i64>,
) -> Result<AccountId, ApiError> {
    if let Some(id) = account_id_input {
        return Ok(AccountId(id));
    }
    let pool = std::sync::Arc::clone(s.db_pool());
    let provider = provider.to_string();
    let master_key = std::sync::Arc::clone(s.master_key());
    tokio::task::spawn_blocking(move || {
        let w = pool
            .try_writer_for(openproxy_db::conn::ADMIN_LOCK_TIMEOUT)
            .ok_or_else(|| ApiError(CoreError::Internal("writer lock timeout".into())))?;
        let provider_id = ProviderId::new(&provider);
        core_accounts::create(&w, &provider_id, None, &master_key, None, 10, None).map_err(ApiError)
    })
    .await
    .map_err(|e| ApiError(CoreError::Internal(format!("spawn failed: {e}"))))?
}

fn compute_oauth_expires_at(
    expires_in: Option<u64>,
    access_token: &str,
    provider_impl: &OAuthProviderEnum,
) -> Option<String> {
    if !provider_impl.access_tokens_expire() {
        return None;
    }
    openproxy_adapters::jwt::expires_at_for_token(expires_in, access_token)
}

pub(crate) async fn save_oauth_token_and_notify(
    s: &AppState,
    provider: &str,
    provider_impl: &OAuthProviderEnum,
    account_id: AccountId,
    token: &TokenResponse,
    custom_provider_specific: Option<String>,
) -> Result<(), ApiError> {
    let expires_at = compute_oauth_expires_at(token.expires_in, &token.access_token, provider_impl);
    let provider_specific =
        custom_provider_specific.or_else(|| provider_impl.provider_specific_from_token(token));
    let email = provider_impl.email_from_token(token);

    tokio::task::spawn_blocking({
        let pool = std::sync::Arc::clone(s.db_pool());
        let master_key = std::sync::Arc::clone(s.master_key());
        let access_token = token.access_token.clone();
        let refresh_token = token.refresh_token.clone();
        let token_type = token.token_type.clone();
        let expires_at = expires_at.clone();
        let scope = token.scope.clone();
        let provider_specific = provider_specific.clone();
        let email = email.clone();
        move || -> Result<(), CoreError> {
            let w = pool
                .try_writer_for(openproxy_db::conn::ADMIN_LOCK_TIMEOUT)
                .ok_or_else(|| CoreError::Internal("writer lock timeout".into()))?;
            openproxy_core::accounts::store_oauth_tokens(
                &w,
                account_id,
                &master_key,
                openproxy_core::accounts::StoreOAuthTokensParams {
                    access_token: &access_token,
                    refresh_token: refresh_token.as_deref(),
                    token_type: &token_type,
                    expires_at: expires_at.as_deref(),
                    scope: scope.as_deref(),
                    provider_specific: provider_specific.as_deref(),
                    email: email.as_deref(),
                },
            )?;
            openproxy_core::accounts::set_health(
                &w,
                account_id,
                openproxy_types::HealthStatus::Healthy,
            )?;
            Ok(())
        }
    })
    .await
    .map_err(|e| ApiError(CoreError::Internal(format!("spawn failed: {e}"))))?
    .map_err(ApiError)?;

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
