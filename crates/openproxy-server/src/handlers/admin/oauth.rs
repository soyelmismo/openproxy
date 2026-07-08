use super::*;
use axum::{
    Json,
    extract::{Path, Query, State},
};

use openproxy_core::accounts as core_accounts;
use openproxy_core::oauth::OAuthProvider;

pub async fn oauth_authorize(
    State(s): State<AppState>,
    Path(provider): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let registry = s.oauth_provider_registry();
        let provider_impl = registry.get(&provider).ok_or_else(|| {
            ApiError(CoreError::Validation(format!(
                "provider '{}' does not support OAuth authorize",
                provider
            )))
        })?;

        let flow = provider_impl.flow();
        if flow != openproxy_core::oauth::OAuthFlow::AuthorizationCodePkce
            && flow != openproxy_core::oauth::OAuthFlow::AuthorizationCode
        {
            return Err(ApiError(CoreError::Validation(format!(
                "provider '{}' does not support authorization code flow",
                provider
            ))));
        }

        // Google OAuth requires localhost for native app clients.
        // The user will paste the callback URL manually in the dashboard.
        //
        // Post-F0 single-binary merge: the dashboard is served by the
        // openproxy server itself (no separate binary), so the OAuth
        // callback page lives at `/admin/callback.html` on the server's
        // port. Operators set `OPENPROXY_WEB_PORT` to the server's port
        // (typically 8787) so the upstream provider redirects the browser
        // to the right URL. The env-var name is kept for backwards
        // compatibility with operators who already have it set in their
        // environment; a future breaking-change release could rename it
        // to `OPENPROXY_PORT`.
        let web_port = std::env::var("OPENPROXY_WEB_PORT").unwrap_or_else(|_| "8788".to_string());
        let redirect_uri = format!("http://localhost:{}/admin/callback.html", web_port);

        let (auth_url, code_verifier, _code_challenge) =
            provider_impl.build_auth_url(&redirect_uri).await?;

        Ok(Json(serde_json::json!({
            "authorization_url": auth_url,
            "code_verifier": code_verifier,
            "redirect_uri": redirect_uri,
        })))
    }
    .await;
    body.into()
}

pub async fn oauth_exchange(
    State(s): State<AppState>,
    Path(provider): Path<String>,
    Json(input): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let code = input
            .get("code")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::Validation("missing 'code'".into()))?;
        let code_verifier = input
            .get("code_verifier")
            .and_then(|v| v.as_str())
            .unwrap_or(""); // Optional — not needed for device code flow
        let account_id_input = input.get("account_id").and_then(|v| v.as_i64());
        let redirect_uri = input
            .get("redirect_uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::Validation("missing 'redirect_uri'".into()))?;

        let registry = s.oauth_provider_registry();
        let provider_impl = registry.get(&provider).ok_or_else(|| {
            ApiError(CoreError::Validation(format!(
                "provider '{}' does not support OAuth exchange",
                provider
            )))
        })?;

        let upstream_client = s.upstream_client();
        let token = provider_impl
            .exchange_code(code, code_verifier, upstream_client, redirect_uri)
            .await?;

        // If no account_id provided, create a new account for this OAuth provider.
        let account_id = match account_id_input {
            Some(id) => AccountId(id),
            None => {
                let w = s.db_pool().writer();
                let provider_id = ProviderId::new(&provider);
                core_accounts::create(
                    &w,
                    &provider_id,
                    None, // no API key — OAuth account
                    s.master_key(),
                    None, // label
                    10,   // default priority
                    None, // extra_config_json
                )?
            }
        };
        let expires_at = token.expires_in.map(|secs| {
            (chrono::Utc::now() + chrono::Duration::seconds(secs as i64))
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        });
        {
            let w = s.db_pool().writer();
            let provider_specific = provider_impl.provider_specific_from_token(&token);
            let email = provider_impl.email_from_token(&token);
            openproxy_core::accounts::store_oauth_tokens(
                &w,
                account_id,
                &token.access_token,
                token.refresh_token.as_deref(),
                s.master_key(),
                &token.token_type,
                expires_at.as_deref(),
                token.scope.as_deref(),
                provider_specific.as_deref(),
                email.as_deref(),
            )?;
        }

        // Post-exchange hook. For Antigravity this calls
        // loadCodeAssist / onboardUser to recover the user's
        // projectId; for other PKCE providers it's a no-op.
        // Errors are logged but do not abort the request — the
        // account is still usable for token refresh; the project
        // bootstrap can be retried later.
        if let Err(e) = provider_impl
            .post_exchange(account_id, s.db_pool(), s.master_key(), s.upstream_client())
            .await
        {
            tracing::warn!(
                account = account_id.0,
                provider = %provider,
                error = %e,
                "oauth post_exchange hook failed; account usable without it"
            );
        }

        Ok(Json(serde_json::json!({
            "status": "ok",
            "account_id": account_id.0,
            "token_type": token.token_type,
        })))
    }
    .await;
    body.into()
}

pub async fn oauth_device_code(
    State(s): State<AppState>,
    Path(provider): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let registry = s.oauth_provider_registry();
        let provider_impl = registry.get(&provider).ok_or_else(|| {
            ApiError(CoreError::Validation(format!(
                "provider '{}' does not support device code flow",
                provider
            )))
        })?;

        let upstream_client = s.upstream_client();
        let dar = provider_impl.request_device_code(upstream_client).await?;

        // LOW fix (#12): persist the device code ticket so the
        // dashboard can survive a page refresh between the
        // user-code entry and the polling phase. Without this the
        // upstream `device_code` only lived in the response
        // payload — a reload / state eviction / server restart
        // would force the user to restart the whole flow. See
        // `openproxy_core::oauth_tickets` for the storage shape.
        {
            let w = s.db_pool().writer();
            openproxy_core::oauth_tickets::create_ticket(&w, &provider, &dar)?;
        }

        Ok(Json(serde_json::json!({
            "device_code": dar.device_code,
            "user_code": dar.user_code,
            "verification_uri": dar.verification_uri,
            "verification_uri_complete": dar.verification_uri_complete,
            "expires_in": dar.expires_in,
            "interval": dar.interval,
        })))
    }
    .await;
    body.into()
}

pub async fn oauth_device_poll(
    State(s): State<AppState>,
    Path(provider): Path<String>,
    Json(input): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let device_code = input
            .get("device_code")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::Validation("missing 'device_code'".into()))?;
        let account_id_input = input
            .get("account_id")
            .and_then(|v| v.as_i64());

        // LOW fix (#12): validate the ticket before any upstream
        // call. An expired, consumed, or unknown device_code is
        // rejected here so the dashboard sees a coherent error
        // instead of a confusing upstream "authorization_pending"
        // loop or a silent double-redeem. `lookup_active` does
        // not mutate state, so a stalled poll never burns the
        // ticket — only `mark_consumed` on success.
        {
            let w = s.db_pool().writer();
            match openproxy_core::oauth_tickets::lookup_active(&w, device_code)? {
                openproxy_core::oauth_tickets::TicketStatus::Active(_) => {}
                openproxy_core::oauth_tickets::TicketStatus::Expired => {
                    return Err(ApiError(CoreError::Validation(
                        "device_code has expired; restart the OAuth flow".into(),
                    )));
                }
                openproxy_core::oauth_tickets::TicketStatus::Consumed => {
                    return Err(ApiError(CoreError::NotFound {
                        what: "oauth_device_ticket".into(),
                        id: device_code.into(),
                    }));
                }
                openproxy_core::oauth_tickets::TicketStatus::Unknown => {
                    return Err(ApiError(CoreError::NotFound {
                        what: "oauth_device_ticket".into(),
                        id: device_code.into(),
                    }));
                }
            }
        }

        let registry = s.oauth_provider_registry();
        let provider_impl = registry.get(&provider).ok_or_else(|| {
            ApiError(CoreError::Validation(format!(
                "provider '{}' does not support device code polling",
                provider
            )))
        })?;

        let upstream_client = s.upstream_client();
        match provider_impl
            .poll_device_token(device_code, upstream_client)
            .await?
        {
            Some(token) => {
                // If no account_id provided, create a new account for this OAuth provider.
                let account_id = match account_id_input {
                    Some(id) => AccountId(id),
                    None => {
                        let w = s.db_pool().writer();
                        let provider_id = ProviderId::new(&provider);
                        core_accounts::create(
                            &w,
                            &provider_id,
                            None, // no API key — OAuth account
                            s.master_key(),
                            None,   // label
                            10,     // default priority
                            None,   // extra_config_json
                        )?
                    }
                };
                let expires_at = token.expires_in.map(|secs| {
                    (chrono::Utc::now() + chrono::Duration::seconds(secs as i64))
                        .format("%Y-%m-%dT%H:%M:%SZ")
                        .to_string()
                });

                // For Kiro, recover the OIDC credentials that
                // `request_device_code` stashed in a thread-local
                // cache (60s TTL) and write them to
                // `oauth_provider_specific` so the post-exchange
                // hook + chat executor can find them. The store
                // is a no-op for providers that don't use a
                // dynamic client registration.
                let provider_specific = match provider.as_str() {
                    "kiro" => openproxy_core::oauth_kiro::take_last_client()
                        .map(|(cid, csec)| {
                            serde_json::json!({
                                "client_id": cid,
                                "client_secret": csec,
                                "region": openproxy_core::oauth_kiro::KiroProviderMeta::default().region,
                            })
                            .to_string()
                        }),
                    _ => provider_impl.provider_specific_from_token(&token),
                };
                let email = provider_impl.email_from_token(&token);

                {
                    let w = s.db_pool().writer();
                    openproxy_core::accounts::store_oauth_tokens(
                        &w,
                        account_id,
                        &token.access_token,
                        token.refresh_token.as_deref(),
                        s.master_key(),
                        &token.token_type,
                        expires_at.as_deref(),
                        token.scope.as_deref(),
                        provider_specific.as_deref(),
                        email.as_deref(),
                    )?;
                }

                // LOW fix (#12): single-use enforcement. After a
                // successful exchange the ticket is consumed so a
                // retry (legitimate or replayed) cannot redeem the
                // same device_code twice. The WHERE clause in
                // `mark_consumed` is atomic, so a racing second
                // poll will see the first redeem as Consumed and
                // fail here too.
                if let Err(e) = (|| -> Result<(), ApiError> {
                    let w = s.db_pool().writer();
                    openproxy_core::oauth_tickets::mark_consumed(&w, device_code)
                        .map_err(ApiError)?;
                    Ok(())
                })() {
                    tracing::warn!(
                        device_code = %device_code,
                        error = %e.0,
                        "mark_consumed failed; downstream was already wired — \
                         a replay may now succeed before the next cleanup sweep"
                    );
                }

                // Post-exchange hook. For Kiro this hits
                // ListAvailableProfiles to recover the user's
                // profileArn; the resulting JSON is written to
                // `oauth_provider_specific`. Errors are logged
                // but do not abort the request.
                if let Err(e) = provider_impl
                    .post_exchange(account_id, s.db_pool(), s.master_key(), s.upstream_client())
                    .await
                {
                    tracing::warn!(
                        account = account_id.0,
                        provider = %provider,
                        error = %e,
                        "oauth post_exchange hook failed; account usable without it"
                    );
                }

                Ok(Json(serde_json::json!({
                    "status": "ok",
                    "account_id": account_id.0,
                })))
            }
            None => Ok(Json(serde_json::json!({
                "status": "pending",
            }))),
        }
    }
    .await;
    body.into()
}

pub async fn oauth_callback(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let code = params.get("code").cloned().unwrap_or_default();
    let error = params.get("error").cloned();

    Json(serde_json::json!({
        "code": code,
        "error": error,
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

    let access_token = {
        let conn = s.db_pool().writer();
        match core_accounts::decrypt_access_token(&conn, account.id, s.master_key().as_ref()) {
            Ok(token) => token,
            Err(e) => {
                tracing::warn!(
                    account = account.id.0,
                    provider = %provider_id,
                    error = %e,
                    "oauth refresh-on-demand: failed to decrypt access token"
                );
                return String::new();
            }
        }
    };

    if !core_oauth::oauth_expires_soon(&account, provider_id.as_str()) {
        return access_token;
    }

    let stored_access_token = access_token.clone();
    let refresh_token = {
        let conn = s.db_pool().writer();
        match core_accounts::decrypt_refresh_token(&conn, account.id, s.master_key().as_ref()) {
            Ok(Some(rt)) => rt,
            Ok(None) => return stored_access_token,
            Err(e) => {
                tracing::warn!(
                    account = account.id.0,
                    provider = %provider_id,
                    error = %e,
                    "oauth refresh-on-demand: failed to decrypt refresh token"
                );
                return stored_access_token;
            }
        }
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

    let upstream_client = s.upstream_client();
    match provider
        .refresh_token(
            &refresh_token,
            upstream_client,
            account.id,
            openproxy_core::oauth::DbRef::Pool(s.db_pool().as_ref()),
        )
        .await
    {
        Ok(token) => {
            let expires_at = token.expires_in.map(|secs| {
                (chrono::Utc::now() + chrono::Duration::seconds(secs as i64))
                    .format("%Y-%m-%dT%H:%M:%SZ")
                    .to_string()
            });

            let conn = s.db_pool().writer();
            match core_accounts::store_oauth_tokens(
                &conn,
                account.id,
                &token.access_token,
                token.refresh_token.as_deref(),
                s.master_key().as_ref(),
                &token.token_type,
                expires_at.as_deref(),
                token.scope.as_deref(),
                account.oauth_provider_specific.as_deref(),
                account.email.as_deref(),
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
                    access_token
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                account = account.id.0,
                provider = %provider_id,
                error = %e,
                "oauth refresh-on-demand: token refresh failed"
            );
            access_token
        }
    }
}
