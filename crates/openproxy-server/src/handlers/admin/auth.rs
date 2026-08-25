use super::{ApiError, AppState, CoreError, HeaderMap, IntoResponse};

pub(crate) fn authenticate_admin_ws(
    state: &AppState,
    headers: &HeaderMap,
    query_token: Option<&str>,
) -> Result<(), ApiError> {
    // Dev convenience: when the operator explicitly opts in by setting
    // OPENPROXY_DASHBOARD_AUTH_BYPASS=1 in the server's environment, every
    // admin request is accepted without an Authorization header or query
    // token. The match is on the exact sentinel `1` — NOT "any non-empty
    // value" — so a typo or stray config (e.g. `=false`, `=yes`, `=0`,
    // `=legacy-token`) cannot silently grant full admin access. The match
    // is logged at WARN level so the bypass is visible in production logs
    // and dashboards alerting on auth-bypass are wired correctly.
    // NOTE: The bypass is gated behind debug_assertions so it is
    // completely compiled out in release builds — no attacker can
    // exploit it even if the env var is set.
    #[cfg(debug_assertions)]
    {
        if let Ok(bypass) = std::env::var("OPENPROXY_DASHBOARD_AUTH_BYPASS")
            && bypass == "1"
        {
            tracing::warn!(
                target: "openproxy::security",
                path = ?headers.get("x-original-uri").and_then(|v| v.to_str().ok()),
                method = ?headers.get("x-original-method").and_then(|v| v.to_str().ok()),
                "admin auth bypassed via OPENPROXY_DASHBOARD_AUTH_BYPASS=1 — \
                 every admin endpoint is open. Remove this env var to restore auth."
            );
            return Ok(());
        }
    }

    // Extract token from Authorization header or from query parameter
    let header_token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            use subtle::ConstantTimeEq;
            let b = s.as_bytes();
            if b.len() >= 7 && bool::from(b[..7].ct_eq(b"Bearer ")) {
                Some(s[7..].trim())
            } else {
                None
            }
        });

    let t = header_token.or(query_token).ok_or_else(|| {
        ApiError(CoreError::Auth(
            "missing authorization header or token query parameter".into(),
        ))
    })?;

    if t.is_empty() {
        return Err(ApiError(CoreError::Auth("invalid token".into())));
    }

    crate::middleware::auth::verify_key_credentials(state, t, "manage")?;
    Ok(())
}

pub async fn admin_auth_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if let Err(e) = authenticate_admin_ws(&state, req.headers(), None) {
        return e.into_response();
    }
    next.run(req).await
}
