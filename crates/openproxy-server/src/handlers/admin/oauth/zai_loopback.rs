//! The zai (AutoClaw / ZCode) loopback callback listener.
//!
//! Security (audit H-1): the listener used to bind one of four FIXED ports
//! (18432/19654/19723/53699) from the `authorize` handler with a
//! first-connection-wins accept. Any other local process could squat those
//! ports ahead of the server and capture the `code`+`state` the user's
//! browser redirects to `http://localhost:<fixed>/auth/callback-zai`.
//!
//! The listener now:
//!  * binds `127.0.0.1:0` — an ephemeral port the OS guarantees is free at
//!    bind time, so there is nothing to squat;
//!  * is created by the endpoint that actually negotiates the callback with
//!    the upstream (`verify-captcha`, whose `navigate_uri` is what the
//!    browser is told to redirect to), so the port in the URI and the port
//!    being listened on can never drift apart;
//!  * validates the echoed `state` against the server-issued record before
//!    exchanging anything (single-use consume), so a code from a flow this
//!    server never initiated is rejected.

use super::{resolve_or_create_oauth_account, save_oauth_token_and_notify};
use crate::handlers::admin::AppState;

use openproxy_core::oauth::OAuthProvider;

/// Bind the loopback listener on an ephemeral port.
///
/// Returns the listener and the port it actually got; the caller embeds this
/// port in the `navigate_uri` sent upstream, so the browser's redirect and
/// the listener are guaranteed to agree.
pub async fn bind_ephemeral() -> std::io::Result<(tokio::net::TcpListener, u16)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener
        .local_addr()
        .map(|addr| addr.port())
        .map_err(|e| std::io::Error::other(format!("local_addr on bound listener: {e}")))?;
    Ok((listener, port))
}

/// The callback URI this server advertises for a listener bound on `port`.
pub fn navigate_uri_for_port(port: u16) -> String {
    format!("http://localhost:{port}/auth/callback-zai")
}

/// Serve exactly one callback connection on `listener`.
///
/// The connection is expected to be the browser redirect carrying
/// `?code=..&state=..`. The state is consumed server-side (single use,
/// TTL-bound, provider-bound); on success the code is exchanged with the
/// verifier+redirect persisted at authorize time and the tokens are stored.
/// Any failure is logged and the browser still gets the friendly HTML (the
/// operator's dashboard refreshes the account list independently).
pub fn spawn_single_shot_handler(
    state: AppState,
    listener: tokio::net::TcpListener,
    port: u16,
    provider: String,
) {
    tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 4096];
        let Ok(n) = stream.read(&mut buf).await else {
            return;
        };
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
            let Ok(account_id) = resolve_or_create_oauth_account(&state, &provider, None).await
            else {
                respond_success_html(&mut stream).await;
                return;
            };
            let registry = state.oauth_provider_registry();
            let Some(provider_impl) = registry.get(&provider) else {
                respond_success_html(&mut stream).await;
                return;
            };
            let redirect_uri = navigate_uri_for_port(port);
            // Security (audit H-1 / OP-19): the loopback listener binds the
            // exchange to the state THIS server generated. A code from a flow
            // we never initiated (redirected to any other local port, or
            // replayed) is rejected here.
            let Some((code_verifier_val, _state_redirect)) =
                state.oauth_states().consume(&provider, state_val.trim())
            else {
                tracing::warn!(
                    provider = %provider,
                    "zai loopback callback rejected: unknown/expired/used state"
                );
                respond_success_html(&mut stream).await;
                return;
            };
            match provider_impl
                .exchange_code(
                    actual_code,
                    &code_verifier_val,
                    state.upstream_client(),
                    &redirect_uri,
                )
                .await
            {
                Ok(token) => {
                    if let Err(e) = save_oauth_token_and_notify(
                        &state,
                        &provider,
                        &provider_impl,
                        account_id,
                        &token,
                        None,
                    )
                    .await
                    {
                        tracing::error!("zai loopback exchange: saving token failed: {e}");
                    }
                }
                Err(e) => {
                    tracing::warn!("zai loopback exchange failed: {e}");
                }
            }
        }

        respond_success_html(&mut stream).await;
    });
}

async fn respond_success_html(stream: &mut tokio::net::TcpStream) {
    use tokio::io::AsyncWriteExt;
    let html = "<!doctype html><html><body style='font-family:sans-serif;text-align:center;padding:50px;'><h2>Authorization successful!</h2><p>You can close this tab and return to OpenProxy.</p><script>window.close();</script></body></html>";
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        html.len(),
        html
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[cfg(test)]
#[path = "zai_loopback_tests.rs"]
mod zai_loopback_tests;
