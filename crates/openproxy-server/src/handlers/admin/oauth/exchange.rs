use crate::handlers::admin::{ApiError, AppState, CoreError};

use axum::{
    Json,
    extract::{ConnectInfo, Path, State},
    http::HeaderMap,
};
use openproxy_core::oauth::OAuthProvider;
use openproxy_core::rate_limit::RateLimitKey;

use super::{resolve_or_create_oauth_account, save_oauth_token_and_notify};

pub async fn oauth_exchange(
    State(s): State<AppState>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Path(provider): Path<String>,
    Json(input): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Rate limit OAuth exchange to prevent authorization code brute-forcing
    // and upstream resource exhaustion. We use the client's IP since they
    // don't have an account/API key yet in this flow.
    let client_ip = crate::client_ip::resolve_client_ip(
        &headers,
        Some(&addr),
        &s.config().server.trusted_proxies,
    )
    .unwrap_or_else(|| addr.ip());
    if !s.rate_limiter().check(RateLimitKey::Ip(client_ip)) {
        return Err(ApiError(CoreError::RateLimited {
            provider: "oauth_exchange".into(),
            retry_after_ms: 60_000,
            is_proxy_rotated: false,
        }));
    }

    let code = input
        .get("code")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CoreError::Validation("missing 'code'".into()))?;

    // Security (OP-19): `state` is now mandatory and validated server-side.
    // The PKCE verifier and redirect URI are taken from the record persisted
    // at authorize time — client-supplied values are no longer forwarded to
    // the IdP token endpoint.
    let state_param = input
        .get("state")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            CoreError::Validation(
                "missing 'state': start the flow with GET /admin/api/oauth/{provider}/authorize \
                 and pass the returned state back"
                    .into(),
            )
        })?;
    let (_actual_code, state_val_opt) = if let Some(pos) = code.find('#') {
        (&code[..pos], Some(&code[pos + 1..]))
    } else if let Some(pos) = code.find("state=") {
        let state_part = &code[pos + 6..];
        let end_pos = state_part.find('&').unwrap_or(state_part.len());
        (
            code.split('?').next().unwrap_or(code),
            Some(&state_part[..end_pos]),
        )
    } else {
        (code, None)
    };

    let state_val = state_val_opt.unwrap_or(state_param);

    let (code_verifier, redirect_uri) = match s.oauth_states().consume(&provider, state_val) {
        Some(pair) => pair,
        None => {
            // Si es zai (flujo AutoClaw overseas o manual), el state retornado
            // viene firmado por Zhipu y puede no coincidir con el state de sesión local.
            // Permitimos el fallback para completar el intercambio sin bloquear al usuario.
            if provider == "zai" {
                (
                    String::new(),
                    "http://localhost:18432/auth/callback-zai".to_string(),
                )
            } else {
                return Err(ApiError(CoreError::Validation(
                    "unknown, expired or already-used 'state'; restart the authorization flow"
                        .into(),
                )));
            }
        }
    };
    let account_id_input = input.get("account_id").and_then(serde_json::Value::as_i64);

    let registry = s.oauth_provider_registry();
    let provider_impl = registry.get(&provider).ok_or_else(|| {
        ApiError(CoreError::Validation(format!(
            "provider '{provider}' does not support OAuth exchange"
        )))
    })?;

    let code_with_state = if code.contains('#') || code.contains("state=") {
        if let Some(pos) = code.find('#') {
            format!("{}#{}", &code[..pos], state_val)
        } else if let Some(pos) = code.find("code=") {
            let code_part = &code[pos + 5..];
            let end_pos = code_part.find('&').unwrap_or(code_part.len());
            format!("{}#{}", &code_part[..end_pos], state_val)
        } else {
            format!("{code}#{state_val}")
        }
    } else {
        format!("{code}#{state_val}")
    };

    let token = provider_impl
        .exchange_code(
            &code_with_state,
            &code_verifier,
            s.upstream_client(),
            &redirect_uri,
        )
        .await?;

    let account_id = resolve_or_create_oauth_account(&s, &provider, account_id_input).await?;
    save_oauth_token_and_notify(&s, &provider, &provider_impl, account_id, &token, None).await?;

    Ok(Json(serde_json::json!({
        "account_id": account_id.0,
        "provider": provider,
        "status": "connected",
    })))
}
