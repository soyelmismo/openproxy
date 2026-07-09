use super::*;

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

    let key_hash = core_api_keys::hash_key(t);
    // Auth is a SELECT by hash — use the READER so admin requests don't
    // serialize through the writer mutex. The reader has its own
    // `Mutex<Connection>` (see `db::conn::DbPool`), so auth no longer
    // contends with `cost::record` writes or admin mutations.
    let r = state.db_pool().reader();
    let key = match core_api_keys::get_by_hash(&r, &key_hash).map_err(ApiError)? {
        Some(k) => k,
        None => {
            return Err(ApiError(CoreError::Auth("invalid api key".into())));
        }
    };

    if !key.is_active {
        return Err(ApiError(CoreError::Auth(
            "api key revoked or inactive".into(),
        )));
    }

    if let Some(exp) = &key.expires_at {
        // LOW fix (#15): previously a lexicographic string
        // comparison against `now.format("%Y-%m-%d %H:%M:%S")`. The
        // stored value uses `%Y-%m-%dT%H:%M:%SZ` (RFC3339-ish), so
        // the `T` (ASCII 84) sorted AFTER the space (ASCII 32) and
        // every key with `expires_at` was effectively never-expiring.
        // The helper parses both sides through `chrono` so the
        // check now means what it says.
        if core_api_keys::is_expired(Some(exp), chrono::Utc::now())
            .map_err(|e| ApiError(CoreError::Internal(format!("expires_at check: {e}"))))?
        {
            return Err(ApiError(CoreError::Auth("api key expired".into())));
        }
    }

    if !key.scopes.iter().any(|s| s == "manage") {
        return Err(ApiError(CoreError::Auth(
            "api key lacks required scope".into(),
        )));
    }

    // Fire-and-forget the `last_used_at` UPDATE on a blocking thread.
    // The auth path no longer blocks on acquiring the writer mutex,
    // and `touch_last_used` already throttles itself to 5-minute
    // writes (see `LAST_USED_THROTTLE_SECS` in `api_keys.rs`), so the
    // extra writer acquisition here only happens once per key per
    // five minutes under steady load.
    let pool = Arc::clone(state.db_pool());
    let key_id = key.id;
    tokio::task::spawn_blocking(move || {
        let w = pool.writer();
        let _ = core_api_keys::touch_last_used(&w, key_id);
    });

    Ok(())
}

pub async fn admin_auth_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let headers = req.headers().clone();
    if let Err(e) = authenticate_admin_ws(&state, &headers, None) {
        return e.into_response();
    }
    next.run(req).await
}
