use super::{ApiError, AppState, CoreError, HeaderMap, IntoResponse};
use std::net::SocketAddr;

#[cfg(debug_assertions)]
fn check_dev_auth_bypass(
    headers: &HeaderMap,
    remote_addr: Option<&SocketAddr>,
) -> Result<bool, ApiError> {
    let Ok(bypass) = std::env::var("OPENPROXY_DASHBOARD_AUTH_BYPASS") else {
        return Ok(false);
    };
    if bypass != "1" {
        return Ok(false);
    }
    if let Some(addr) = remote_addr
        && !addr.ip().is_loopback()
    {
        tracing::error!(
            target: "openproxy::security",
            ip = %addr.ip(),
            "attempted to use OPENPROXY_DASHBOARD_AUTH_BYPASS from non-loopback IP"
        );
        return Err(ApiError(CoreError::Auth(
            "unauthorized IP for dev bypass".into(),
        )));
    }
    tracing::warn!(
        target: "openproxy::security",
        path = ?headers.get("x-original-uri").and_then(|v| v.to_str().ok()),
        method = ?headers.get("x-original-method").and_then(|v| v.to_str().ok()),
        "admin auth bypassed via OPENPROXY_DASHBOARD_AUTH_BYPASS=1 — \
         every admin endpoint is open. Remove this env var to restore auth."
    );
    Ok(true)
}

fn extract_admin_token<'a>(
    headers: &'a HeaderMap,
    query_token: Option<&'a str>,
) -> Result<&'a str, ApiError> {
    let header_token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::trim);

    let t = header_token.or(query_token).ok_or_else(|| {
        ApiError(CoreError::Auth(
            "missing authorization header or token query parameter".into(),
        ))
    })?;

    if t.is_empty() {
        return Err(ApiError(CoreError::Auth("invalid token".into())));
    }
    Ok(t)
}

pub(crate) fn authenticate_admin_ws(
    state: &AppState,
    headers: &HeaderMap,
    query_token: Option<&str>,
    _remote_addr: Option<&SocketAddr>,
) -> Result<(), ApiError> {
    #[cfg(debug_assertions)]
    if check_dev_auth_bypass(headers, _remote_addr)? {
        return Ok(());
    }

    let token = extract_admin_token(headers, query_token)?;
    crate::middleware::auth::verify_key_credentials(state, token, "manage")?;
    Ok(())
}

pub async fn admin_auth_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if let Err(e) = authenticate_admin_ws(&state, req.headers(), None, Some(&addr)) {
        return e.into_response();
    }
    next.run(req).await
}
