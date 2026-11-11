use crate::handlers::admin::{ApiError, AppState, CoreError};

use axum::{
    Json,
    extract::{Path, State},
};
use openproxy_core::oauth::{OAuthProvider, OAuthProviderEnum};

use super::{get_oauth_redirect_uri, resolve_or_create_oauth_account, save_oauth_token_and_notify};

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
    let redirect_uri = get_oauth_redirect_uri(&s);
    let (auth_url, code_verifier, effective_redirect_uri, state) =
        provider_impl.build_auth_url(&redirect_uri).await?;
    let redirect_uri = if effective_redirect_uri.is_empty() {
        redirect_uri
    } else {
        effective_redirect_uri
    };

    // Security (OP-19): persist the generated state with the verifier and
    // redirect URI actually used, so the exchange can validate `state` and
    // stop trusting client-supplied redirect_uri / code_verifier.
    s.oauth_states()
        .insert(&provider, &state, &code_verifier, &redirect_uri);

    if provider == "zai" {
        let bind_ports = [18432, 19654, 19723, 53699];
        for port in bind_ports {
            let addr = format!("127.0.0.1:{port}");
            if let Ok(listener) = tokio::net::TcpListener::bind(&addr).await {
                let s_clone = s.clone();
                let provider_name = provider.clone();
                tokio::spawn(async move {
                    if let Ok((mut stream, _)) = listener.accept().await {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 4096];
                        if let Ok(n) = stream.read(&mut buf).await {
                            let req_str = String::from_utf8_lossy(&buf[..n]);
                            let mut code = None;
                            let mut state_param = None;
                            if let Some(first_line) = req_str.lines().next()
                                && let Some(target) = first_line.split_whitespace().nth(1)
                                && let Some(query_idx) = target.find('?')
                            {
                                for pair in target[query_idx + 1..].split('&') {
                                    if let Some((k, v)) = pair.split_once('=') {
                                        if k == "code" {
                                            code = Some(v.to_string());
                                        } else if k == "state" {
                                            state_param = Some(v.to_string());
                                        }
                                    }
                                }
                            }

                            if let (Some(code_val), Some(state_val)) = (code, state_param) {
                                let actual_code = code_val.trim();
                                let _effective_state = state_val.trim();
                                let Ok(account_id) =
                                    resolve_or_create_oauth_account(&s_clone, &provider_name, None)
                                        .await
                                else {
                                    return;
                                };
                                let registry = s_clone.oauth_provider_registry();
                                let Some(provider_impl) = registry.get(&provider_name) else {
                                    return;
                                };
                                let redirect_uri =
                                    format!("http://localhost:{port}/auth/callback-zai");
                                let code_verifier_val = "";
                                if let Ok(token) = provider_impl
                                    .exchange_code(
                                        actual_code,
                                        code_verifier_val,
                                        s_clone.upstream_client(),
                                        &redirect_uri,
                                    )
                                    .await
                                {
                                    let _ = save_oauth_token_and_notify(
                                        &s_clone,
                                        &provider_name,
                                        &provider_impl,
                                        account_id,
                                        &token,
                                        None,
                                    )
                                    .await;
                                }
                            }

                            let html = "<!doctype html><html><body style='font-family:sans-serif;text-align:center;padding:50px;'><h2>Authorization successful!</h2><p>You can close this tab and return to OpenProxy.</p><script>window.close();</script></body></html>";
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                html.len(),
                                html
                            );
                            let _ = stream.write_all(resp.as_bytes()).await;
                        }
                    }
                });
                break;
            }
        }
    }

    Ok(Json(serde_json::json!({
        "authorization_url": auth_url,
        "code_verifier": code_verifier,
        "redirect_uri": redirect_uri,
        "state": state,
    })))
}
