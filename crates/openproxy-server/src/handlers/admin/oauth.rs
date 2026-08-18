use super::{AccountId, ApiError, AppState, CoreError, ProviderId, core_oauth};
use axum::{
    Json,
    extract::{Path, Query, State},
};

use openproxy_core::accounts as core_accounts;
use openproxy_core::oauth::OAuthProvider;

pub async fn oauth_authorize(
    State(s): State<AppState>,
    Path(provider): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let res: Result<Json<serde_json::Value>, crate::error::ApiError> = async move {
        let registry = s.oauth_provider_registry();
        let provider_impl = registry.get(&provider).ok_or_else(|| {
            ApiError(CoreError::Validation(format!(
                "provider '{provider}' does not support OAuth authorize"
            )))
        })?;

        let flow = provider_impl.flow();
        if flow != openproxy_core::oauth::OAuthFlow::AuthorizationCodePkce
            && flow != openproxy_core::oauth::OAuthFlow::AuthorizationCode
        {
            return Err(ApiError(CoreError::Validation(format!(
                "provider '{provider}' does not support authorization code flow"
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
        let redirect_uri = format!("http://localhost:{web_port}/admin/callback.html");

        let (auth_url, code_verifier, _, state) =
            provider_impl.build_auth_url(&redirect_uri).await?;

        Ok(Json(serde_json::json!({
            "authorization_url": auth_url,
            "code_verifier": code_verifier,
            "redirect_uri": redirect_uri,
            "state": state,
        })))
    }
    .await;
    res
}

pub async fn oauth_exchange(
    State(s): State<AppState>,
    Path(provider): Path<String>,
    Json(input): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let res: Result<Json<serde_json::Value>, crate::error::ApiError> = async move {
        let code = input
            .get("code")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::Validation("missing 'code'".into()))?;
        let code_verifier = input
            .get("code_verifier")
            .and_then(|v| v.as_str())
            .unwrap_or(""); // Optional — not needed for device code flow
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
        let upstream_client = s.upstream_client();
        let token = provider_impl
            .exchange_code(code, code_verifier, upstream_client, redirect_uri)
            .await?;

        // If no account_id provided, create a new account for this OAuth provider.
        let account_id = match account_id_input {
            Some(id) => AccountId(id),
            None => tokio::task::block_in_place(|| {
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
                )
            })?,
        };
        let expires_at = token.expires_in.map(|secs| {
            (chrono::Utc::now() + chrono::Duration::seconds(secs as i64))
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        });
        {
            let provider_specific = provider_impl.provider_specific_from_token(&token);
            let email = provider_impl.email_from_token(&token);

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
        }

        // LOW fix (#11): fire the post-exchange hook if the
        // provider supports one. For KiRO (and future providers)
        // this auto-creates an initial catalog model and probes
        // the upstream for its region / profile ARN. Errors are
        // non-fatal — the operator already has the account row
        // so we log at WARN and return a normal JSON response.
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
            s,
            provider.clone(),
            Some(account_id.0),
        );

        Ok(Json(serde_json::json!({
            "account_id": account_id.0,
            "provider": provider,
            "status": "connected",
        })))
    }
    .await;
    res
}

pub async fn oauth_device_code(
    State(s): State<AppState>,
    Path(provider): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let res: Result<Json<serde_json::Value>, crate::error::ApiError> = async move {
        let registry = s.oauth_provider_registry();
        let provider_impl = registry.get(&provider).ok_or_else(|| {
            ApiError(CoreError::Validation(format!(
                "provider '{provider}' does not support device code authorization"
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
        // `openproxy_core::oauth::tickets` for the storage shape.
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
    .await;
    res
}

pub async fn oauth_device_poll(
    State(s): State<AppState>,
    Path(provider): Path<String>,
    Json(input): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let res: Result<Json<serde_json::Value>, crate::error::ApiError> = async move {
        let device_code = input
            .get("device_code")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::Validation("missing 'device_code'".into()))?;
        let account_id_input = input
            .get("account_id")
            .and_then(serde_json::Value::as_i64);

        // LOW fix (#12): validate the ticket before any upstream
        // call. An expired, consumed, or unknown device_code is
        // rejected here so the dashboard sees a coherent error
        // instead of a confusing upstream "authorization_pending"
        // loop or a silent double-redeem. `lookup_active` does
        // not mutate state, so a stalled poll never burns the
        // ticket — only `mark_consumed` on success.
        {
            tokio::task::block_in_place(|| {
                let w = s.db_pool().writer();
                match openproxy_core::oauth::tickets::lookup_active(&w, device_code)? {
                    openproxy_core::oauth::tickets::TicketStatus::Active(_) => Ok(()),
                    openproxy_core::oauth::tickets::TicketStatus::Expired => {
                        Err(ApiError(CoreError::Validation(
                            "device_code has expired; restart the OAuth flow".into(),
                        )))
                    }
                    openproxy_core::oauth::tickets::TicketStatus::Consumed
                    | openproxy_core::oauth::tickets::TicketStatus::Unknown => {
                        Err(ApiError(CoreError::NotFound {
                            what: "oauth_device_ticket".into(),
                            id: device_code.to_string(),
                        }))
                    }
                }
            })?;
        }

        let registry = s.oauth_provider_registry();
        let provider_impl = registry.get(&provider).ok_or_else(|| {
            ApiError(CoreError::Validation(format!(
                "provider '{provider}' does not support device code polling"
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
                    None => tokio::task::block_in_place(|| {
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
                        )
                    })?,
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
                    "kiro" => openproxy_core::oauth::kiro::take_last_client()
                        .map(|(cid, csec)| {
                            serde_json::json!({
                                "client_id": cid,
                                "client_secret": csec,
                                "region": openproxy_core::oauth::kiro::KiroProviderMeta::default().region,
                            })
                            .to_string()
                        }),
                    _ => provider_impl.provider_specific_from_token(&token),
                };
                let email = provider_impl.email_from_token(&token);

                {
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
                }

                // LOW fix (#12): single-use enforcement. After a
                // successful exchange the ticket is consumed so a
                // retry (legitimate or replayed) cannot redeem the
                // same device_code twice. The WHERE clause in
                // `mark_consumed` is atomic, so a racing second
                // poll will see the first redeem as Consumed and
                // fail here too.
                if let Err(e) = tokio::task::block_in_place(|| -> Result<(), ApiError> {
                    let w = s.db_pool().writer();
                    openproxy_core::oauth::tickets::mark_consumed(&w, device_code)
                        .map_err(ApiError)?;
                    Ok(())
                }) {
                    tracing::warn!(
                        device_code = %device_code,
                        error = %e.0,
                        "mark_consumed failed; downstream was already wired — \
                         a replay may now succeed before the next cleanup sweep",
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

                super::providers::spawn_background_provider_refresh(
                    s.clone(),
                    provider.clone(),
                    Some(account_id.0),
                );

                Ok(Json(serde_json::json!({
                    "status": "ok",
                    "account_id": account_id.0,
                })))
            }
            None => Ok(Json(serde_json::json!({
                "status": "pending",
            }))),
        }
    }.await;
    res
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

    let refresh_token = {
        let conn = s.db_pool().writer();
        match core_accounts::decrypt_refresh_token(&conn, account.id, s.master_key().as_ref()) {
            Ok(Some(rt)) => rt,
            Ok(None) => return access_token,
            Err(e) => {
                tracing::warn!(
                    account = account.id.0,
                    provider = %provider_id,
                    error = %e,
                    "oauth refresh-on-demand: failed to decrypt refresh token"
                );
                return access_token;
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
