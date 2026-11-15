use super::{ApiError, AppState, CoreError, HeaderMap, IntoResponse};
use openproxy_core::api_keys as core_api_keys;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

/// Who is calling an admin endpoint, as resolved by [`admin_auth_middleware`],
/// stored in request extensions so handlers can bind derived credentials (WS
/// tickets) to the key and write attributable audit records for secret reads.
/// `key` is `None` only under the debug-build dev bypass.
#[derive(Clone, Debug)]
pub struct AdminIdentity {
    pub key: Option<Arc<core_api_keys::ApiKey>>,
    pub remote_addr: Option<SocketAddr>,
    pub client_ip: Option<IpAddr>,
}

impl AdminIdentity {
    /// Numeric key id for audit logs (`None` under dev bypass).
    pub fn key_id(&self) -> Option<i64> {
        self.key.as_ref().map(|k| k.id.0)
    }

    /// Resolved client IP for audit logs, rate limits, and security tracking.
    pub fn client_ip(&self) -> Option<IpAddr> {
        self.client_ip.or_else(|| self.remote_addr.map(|a| a.ip()))
    }

    /// Whether this identity has full management privileges ("manage" scope or dev bypass).
    pub fn has_manage_scope(&self) -> bool {
        match &self.key {
            None => true, // dev bypass has full rights
            Some(key) => key.scopes.iter().any(|s| s == "manage"),
        }
    }

    /// Whether this identity has a specific scope.
    pub fn has_scope(&self, scope: &str) -> bool {
        match &self.key {
            None => true, // dev bypass
            Some(key) => key.scopes.iter().any(|s| s == scope),
        }
    }
}

/// Extractor alias for handlers: the identity resolved by
/// [`admin_auth_middleware`]. `Option` because a handler may be mounted outside
/// the middleware in tests; production admin routes always have it.
pub(crate) type Identity = Option<axum::Extension<AdminIdentity>>;

/// Profile metadata of the authenticated caller returned by `GET /admin/api/auth/me`.
#[derive(Debug, serde::Serialize)]
pub struct AuthMeResponse {
    pub key_id: Option<i64>,
    pub key_prefix: Option<String>,
    pub label: Option<String>,
    pub scopes: Vec<String>,
    pub is_admin: bool,
}

pub async fn auth_me(
    identity: Option<axum::Extension<AdminIdentity>>,
) -> Result<axum::Json<AuthMeResponse>, ApiError> {
    let id = identity.as_ref().map(|axum::Extension(i)| i);

    match id {
        Some(identity) => match &identity.key {
            Some(key) => Ok(axum::Json(AuthMeResponse {
                key_id: Some(key.id.0),
                key_prefix: key.key_prefix.clone(),
                label: key.label.clone(),
                scopes: key.scopes.clone(),
                is_admin: identity.has_manage_scope(),
            })),
            None => Ok(axum::Json(AuthMeResponse {
                key_id: None,
                key_prefix: None,
                label: Some("dev_bypass".into()),
                scopes: vec!["manage".into(), "analytics".into(), "chat".into()],
                is_admin: true,
            })),
        },
        None => Err(ApiError(CoreError::Auth("unauthenticated".into()))),
    }
}

/// Audit record written whenever an admin endpoint hands out (or writes to disk) a
/// decrypted secret. WARN on the dedicated `openproxy::security::audit` target so
/// operators can route it to a separate sink and alert on unexpected
/// `key_id`/`ip` pairs.
pub(crate) fn audit_secret_read(identity: &Identity, secret_kind: &str, subject: &str) {
    let id = identity.as_ref().map(|axum::Extension(i)| i);
    tracing::warn!(
        target: "openproxy::security::audit",
        key_id = id.and_then(AdminIdentity::key_id),
        key_prefix = id
            .and_then(|i| i.key.as_ref())
            .and_then(|k| k.key_prefix.as_deref()),
        ip = id.and_then(AdminIdentity::client_ip).map(|a| a.to_string()),
        secret = secret_kind,
        subject,
        "secret disclosed via admin api"
    );
}

/// Dev-only admin auth bypass (OP-20).
///
/// Gated behind the non-default `dev-auth-bypass` cargo feature instead of
/// `debug_assertions`: a plain `cargo run` (debug build) must NOT ship a
/// switch that disables the ENTIRE admin authentication surface — on a
/// shared host any local user could flip `OPENPROXY_DASHBOARD_AUTH_BYPASS=1`
/// and get full admin access. Opting in now requires building with
/// `--features dev-auth-bypass` explicitly.
#[cfg(feature = "dev-auth-bypass")]
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
    // The bypass is a local-development convenience. Refuse it outright
    // when the request came through a reverse proxy: behind a proxy on
    // the same host every remote client looks like loopback at the TCP
    // layer, so the peer-address check below would be meaningless.
    if headers.contains_key("x-forwarded-for")
        || headers.contains_key("forwarded")
        || headers.contains_key("x-real-ip")
    {
        tracing::error!(
            target: "openproxy::security",
            "OPENPROXY_DASHBOARD_AUTH_BYPASS refused: request carries proxy forwarding headers"
        );
        return Err(ApiError(CoreError::Auth(
            "dev bypass not available behind a proxy".into(),
        )));
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

/// Pull the Bearer token out of the `Authorization` header.
///
/// Credentials are accepted from headers ONLY — never query-string tokens, not
/// even for WebSocket upgrades, because every reverse proxy logs the request line
/// and a long-lived key would land in plaintext in access logs. Browsers that
/// cannot set headers on `new WebSocket()` use the single-use ticket flow
/// ([`authenticate_admin_ws`], `state::WsTicketStore`).
fn extract_bearer_token(headers: &HeaderMap) -> Result<Option<&str>, ApiError> {
    let Some(raw) = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::trim)
    else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Err(ApiError(CoreError::Auth("invalid token".into())));
    }
    Ok(Some(raw))
}

/// Authenticate an admin REST request from its headers.
///
/// Returns the resolved identity; `key` is `None` only under the
/// debug-build dev bypass.
pub(crate) async fn authenticate_admin(
    state: &AppState,
    headers: &HeaderMap,
    remote_addr: Option<&SocketAddr>,
) -> Result<AdminIdentity, ApiError> {
    let client_ip = crate::client_ip::resolve_client_ip(
        headers,
        remote_addr,
        &state.config().server.trusted_proxies,
    );
    let effective_remote_addr = client_ip
        .zip(remote_addr)
        .map(|(ip, peer)| SocketAddr::new(ip, peer.port()))
        .or_else(|| remote_addr.copied());

    #[cfg(feature = "dev-auth-bypass")]
    if check_dev_auth_bypass(headers, remote_addr)? {
        return Ok(AdminIdentity {
            key: None,
            remote_addr: effective_remote_addr,
            client_ip,
        });
    }

    let token = extract_bearer_token(headers)?
        .ok_or_else(|| ApiError(CoreError::Auth("missing authorization header".into())))?;
    // Allowed scopes for admin endpoints: full "manage", read-only "analytics"/
    // "read", or the customer "chat" scope a checkout-minted key carries. The
    // endpoint allowlist in `client_allowed` is what actually bounds a
    // non-manage key: it can only read its own plan and its own tenanted
    // usage, never an operator route.
    let key = crate::middleware::auth::verify_key_credentials_any_scope(
        state,
        token,
        &["manage", "analytics", "read", "chat"],
    )
    .await?;
    Ok(AdminIdentity {
        key: Some(key),
        remote_addr: effective_remote_addr,
        client_ip,
    })
}

/// Redeem a single-use WS ticket and re-validate the key it was bound to.
async fn authenticate_ws_ticket(
    state: &AppState,
    ticket: &str,
) -> Result<Arc<core_api_keys::ApiKey>, ApiError> {
    let key_id = state
        .ws_tickets()
        .consume(ticket)
        .ok_or_else(|| ApiError(CoreError::Auth("invalid or expired ws ticket".into())))?;
    let key = {
        state
            .db_pool()
            .spawn_read(move |r| core_api_keys::get_by_id(r, key_id))
            .await
            .map_err(|e| {
                tracing::error!(%e, "db error resolving ws ticket key");
                ApiError(CoreError::Auth("invalid api key".into()))
            })?
            .ok_or_else(|| ApiError(CoreError::Auth("invalid api key".into())))?
    };
    crate::middleware::auth::validate_key_record(&key, "manage")?;
    Ok(Arc::new(key))
}

/// Authenticate the `/admin/ws` upgrade (and any handler that wants the
/// same contract). Accepts EITHER `Authorization: Bearer <key>` (CLI /
/// non-browser clients) OR a single-use `?ticket=` minted by
/// `POST /admin/api/ws-ticket` (browsers). Never a raw key in the URL.
pub(crate) async fn authenticate_admin_ws(
    state: &AppState,
    headers: &HeaderMap,
    ticket: Option<&str>,
    remote_addr: Option<&SocketAddr>,
) -> Result<AdminIdentity, ApiError> {
    let client_ip = crate::client_ip::resolve_client_ip(
        headers,
        remote_addr,
        &state.config().server.trusted_proxies,
    );
    let effective_remote_addr = client_ip
        .zip(remote_addr)
        .map(|(ip, peer)| SocketAddr::new(ip, peer.port()))
        .or_else(|| remote_addr.copied());

    #[cfg(feature = "dev-auth-bypass")]
    if check_dev_auth_bypass(headers, remote_addr)? {
        return Ok(AdminIdentity {
            key: None,
            remote_addr: effective_remote_addr,
            client_ip,
        });
    }

    let key = match (extract_bearer_token(headers)?, ticket) {
        (Some(token), _) => {
            crate::middleware::auth::verify_key_credentials(state, token, "manage").await?
        }
        (None, Some(t)) if !t.is_empty() => authenticate_ws_ticket(state, t).await?,
        _ => {
            return Err(ApiError(CoreError::Auth(
                "missing authorization header or ws ticket".into(),
            )));
        }
    };
    Ok(AdminIdentity {
        key: Some(key),
        remote_addr: effective_remote_addr,
        client_ip,
    })
}

/// Client-facing allowlist for keys WITHOUT "manage" (analytics/read scopes).
/// Everything not listed here is operator-only. Method is part of the rule, so
/// a client key can never mutate anything: plan purchase runs on the public
/// checkout with the key as Bearer, never through the admin API.
fn client_allowed(path: &str, method: &axum::http::Method) -> bool {
    // Path without the /admin/api nest prefix (nginx /tokens/ strips are
    // applied before this middleware sees the request).
    let p = path.strip_prefix("/admin/api").unwrap_or(path);
    let get = method == axum::http::Method::GET;
    match p {
        "/auth/me" | "/formats" | "/models/catalog" => get,
        // Minimal self-service usage: spend, tokens, the daily trend and the
        // per-model split of the caller's own key. Latency percentiles, race
        // stats, status distribution, combo health, detailed rows and the
        // provider matrix are operator telemetry: they stay off this list so
        // the payload is never even built for a customer.
        "/usage/summary" | "/usage/by-model" | "/usage/by-day" | "/usage/quota" => get,
        // Self-service plan status (own plan, balance and expiry). Plan,
        // order and gateway management is not listed → operator-only.
        "/credits/quota" => get,
        _ => false,
    }
}

/// Whether the admin-auth throttle lets this request through.
///
/// Security (audit H-11): behind a reverse proxy WITHOUT `trusted_proxies`
/// configured, every Internet client shares the proxy's loopback IP in the
/// failure counter, so 20 anonymous failures locked the legitimate operator
/// out of the whole admin surface for 60 s — renewable forever, a cheap
/// remote DoS of the dashboard and the admin API. The escape hatch: a
/// CACHED valid key still authenticates while its IP is blocked (verified
/// against the in-memory key cache only — no DB — so brute force with
/// unknown keys stays blocked without touching SQLite), and its success
/// clears the budget via `record_success`. Unknown or absent credentials
/// get the early 429 as before.
fn admin_throttle_allows(
    state: &AppState,
    headers: &HeaderMap,
    client_ip: std::net::IpAddr,
) -> bool {
    if !state.admin_limiter().is_blocked(client_ip) {
        return true;
    }
    extract_bearer_token(headers)
        .ok()
        .flatten()
        .is_some_and(|token| {
            let key_hash = openproxy_core::api_keys::hash_key(token);
            state.get_cached_api_key(&key_hash).is_some()
        })
}

pub async fn admin_auth_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    // Security (OP-27): per-IP throttle on FAILED admin authentications. Key
    // entropy makes brute force impractical, but unthrottled attempts were a
    // cheap resource-exhaustion and audit-log-flooding vector.
    let client_ip_for_throttle = crate::client_ip::resolve_client_ip(
        req.headers(),
        Some(&addr),
        &state.config().server.trusted_proxies,
    )
    .unwrap_or_else(|| addr.ip());
    if !admin_throttle_allows(&state, req.headers(), client_ip_for_throttle) {
        return crate::error::ApiError(openproxy_types::CoreError::RateLimited {
            provider: "admin_auth_throttle".into(),
            retry_after_ms: 60_000,
            is_proxy_rotated: false,
        })
        .into_response();
    }

    match authenticate_admin(&state, req.headers(), Some(&addr)).await {
        Ok(identity) => {
            state.admin_limiter().record_success(client_ip_for_throttle);

            // Scope-based endpoint authorization guard (deny by default).
            // "manage" is the operator scope and sees everything. Client keys
            // (analytics/read) get ONLY the explicit allowlist below: their
            // own tenanted analytics and their own credits surface. Every
            // other route — keys, config, providers, accounts, backups,
            // ws-ticket, package/order management — stays operator-only, so a
            // customer key can never reach an admin route no matter which
            // login screen it came through.
            if !identity.has_manage_scope() {
                let path = req.uri().path();
                let method = req.method();
                if !client_allowed(path, method) {
                    tracing::warn!(
                        target: "openproxy::security::audit",
                        key_id = identity.key_id(),
                        ip = identity.client_ip().map(|a| a.to_string()),
                        path = %path,
                        method = %method,
                        "forbidden admin access: key lacks manage scope"
                    );
                    return (
                        axum::http::StatusCode::FORBIDDEN,
                        axum::Json(serde_json::json!({
                            "error": {
                                "code": "forbidden",
                                "message": "forbidden: api key lacks manage scope"
                            }
                        })),
                    )
                        .into_response();
                }
            }

            req.extensions_mut().insert(identity);
        }
        Err(e) => {
            state.admin_limiter().record_failure(client_ip_for_throttle);
            let path = req.uri().path().to_string();
            let method = req.method().to_string();
            let client_ip = crate::client_ip::resolve_client_ip(
                req.headers(),
                Some(&addr),
                &state.config().server.trusted_proxies,
            );

            let lookup_state = state.clone();
            let lookup_headers = req.headers().clone();
            let existing_key = crate::error::run_blocking(move || {
                Ok(extract_bearer_token(&lookup_headers)
                    .ok()
                    .flatten()
                    .and_then(|token| {
                        let key_hash = core_api_keys::hash_key(token);
                        lookup_state.get_cached_api_key(&key_hash).or_else(|| {
                            let r = lookup_state.db_pool().reader();
                            core_api_keys::get_by_hash(&r, &key_hash)
                                .ok()
                                .flatten()
                                .map(Arc::new)
                        })
                    }))
            })
            .await
            .unwrap_or_default();

            if let Some(key) = existing_key {
                tracing::warn!(
                    target: "openproxy::security::audit",
                    key_id = key.id.0,
                    key_prefix = key.key_prefix.as_deref(),
                    key_label = key.label.as_deref(),
                    scopes = ?key.scopes,
                    ip = client_ip.map(|a| a.to_string()),
                    path = %path,
                    method = %method,
                    error = %e.0,
                    "unauthorized admin access attempt with existing api key"
                );
            } else {
                tracing::warn!(
                    target: "openproxy::security::audit",
                    ip = client_ip.map(|a| a.to_string()),
                    path = %path,
                    method = %method,
                    error = %e.0,
                    "admin access attempt rejected"
                );
            }

            return e.into_response();
        }
    }
    next.run(req).await
}

/// Per-IP failed-attempt throttle for the admin API, plus a per-key cap on
/// concurrent dashboard WebSocket streams (OP-27).
///
/// Security: the admin surface previously had no throttle at all — an
/// attacker could hammer `authenticate_admin` (SHA-256 + SQLite lookup per
/// attempt) and open unlimited `/admin/ws` upgrades per key. Key entropy
/// (~190 bits) makes brute force impractical, but the throttle removes the
/// cheap resource-exhaustion and log-flooding paths.
pub struct AdminAuthLimiter {
    /// Shared per-IP failure budget (extracted to
    /// `middleware::auth_throttle::AuthFailureLimiter` so the public `/v1`
    /// throttle reuses the same algorithm and eviction hardening).
    failures: crate::middleware::auth_throttle::AuthFailureLimiter,
    /// key id -> live WebSocket streams
    ws_streams: dashmap::DashMap<openproxy_types::ids::ApiKeyId, usize>,
}

/// Max failed admin authentication attempts per IP per window.
const ADMIN_AUTH_MAX_FAILURES: u32 = 20;
/// Window for the failure counter.
const ADMIN_AUTH_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
/// Max concurrent dashboard WebSocket streams per API key.
const ADMIN_AUTH_MAX_WS_PER_KEY: usize = 32;

impl Default for AdminAuthLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl AdminAuthLimiter {
    pub fn new() -> Self {
        Self {
            failures: crate::middleware::auth_throttle::AuthFailureLimiter::new(
                ADMIN_AUTH_MAX_FAILURES,
                ADMIN_AUTH_WINDOW,
            ),
            ws_streams: dashmap::DashMap::new(),
        }
    }

    /// `true` when `ip` exhausted its failure budget inside the window.
    ///
    /// ROOT FIX (permanent lockout, audit A4-03): the budget only counts
    /// while its window is still live. Without the window check here, 20
    /// failures blocked an IP FOREVER — the early 429 in the middleware made
    /// `record_failure`/`record_success` unreachable, so nothing could ever
    /// reset the counter (only a restart cured it; reproduced with the
    /// legitimate admin still locked out 61 s later).
    pub fn is_blocked(&self, ip: std::net::IpAddr) -> bool {
        self.failures.is_blocked(ip)
    }

    /// Count one failed attempt; resets the window when it expired.
    pub fn record_failure(&self, ip: std::net::IpAddr) {
        self.failures.record_failure(ip);
    }

    /// A successful authentication clears the IP's failure budget.
    pub fn record_success(&self, ip: std::net::IpAddr) {
        self.failures.record_success(ip);
    }

    /// Acquire one WebSocket stream slot for `key`, or `None` when the key
    /// already holds `ADMIN_AUTH_MAX_WS_PER_KEY` live streams.
    pub fn try_acquire_ws(
        self: &std::sync::Arc<Self>,
        key: openproxy_types::ids::ApiKeyId,
    ) -> Option<WsStreamGuard> {
        use dashmap::mapref::entry::Entry;
        // Security: previously this called `clear()` on overflow, which would
        // drop the live-stream counters for ALL keys, letting a key that had
        // already saturated its 32-stream cap grab another 32. We now drop
        // only the keys with zero live streams (i.e. leaked entries from a
        // panic before the guard ran its Drop) — active keys are preserved.
        if self.ws_streams.len() >= 100_000 {
            self.ws_streams.retain(|_, count| *count > 0);
        }
        match self.ws_streams.entry(key) {
            Entry::Occupied(mut o) => {
                if *o.get() >= ADMIN_AUTH_MAX_WS_PER_KEY {
                    None
                } else {
                    *o.get_mut() += 1;
                    Some(WsStreamGuard {
                        key,
                        limiter: std::sync::Arc::clone(self),
                    })
                }
            }
            Entry::Vacant(v) => {
                v.insert(1);
                Some(WsStreamGuard {
                    key,
                    limiter: std::sync::Arc::clone(self),
                })
            }
        }
    }
}

/// Releases one per-key WebSocket stream slot when dropped (i.e. when the
/// stream task finishes).
pub struct WsStreamGuard {
    key: openproxy_types::ids::ApiKeyId,
    limiter: std::sync::Arc<AdminAuthLimiter>,
}

impl Drop for WsStreamGuard {
    fn drop(&mut self) {
        use dashmap::mapref::entry::Entry;
        match self.limiter.ws_streams.entry(self.key) {
            Entry::Occupied(mut o) => {
                if *o.get() <= 1 {
                    o.remove();
                } else {
                    *o.get_mut() -= 1;
                }
            }
            Entry::Vacant(_) => {}
        }
    }
}

#[cfg(test)]
mod limiter_tests {
    use super::*;

    #[test]
    fn admin_auth_failure_throttle_blocks_after_budget() {
        let limiter = std::sync::Arc::new(AdminAuthLimiter::new());
        let ip: std::net::IpAddr = "203.0.113.77".parse().unwrap();
        for _ in 0..ADMIN_AUTH_MAX_FAILURES {
            assert!(!limiter.is_blocked(ip));
            limiter.record_failure(ip);
        }
        assert!(limiter.is_blocked(ip), "blocked at budget");
        limiter.record_success(ip);
        assert!(!limiter.is_blocked(ip), "success clears the budget");
    }

    /// Root-fix regression (audit A4-03): the block must expire with the
    /// window. Before the fix, 20 failures locked an IP out of the admin
    /// surface FOREVER (the middleware's early 429 made record_failure and
    /// record_success unreachable, so no reset path existed short of a
    /// restart).
    #[test]
    fn admin_auth_failure_throttle_expires_with_the_window() {
        let limiter = std::sync::Arc::new(AdminAuthLimiter::new());
        let ip: std::net::IpAddr = "203.0.113.78".parse().unwrap();
        for _ in 0..ADMIN_AUTH_MAX_FAILURES {
            limiter.record_failure(ip);
        }
        assert!(limiter.is_blocked(ip), "blocked at budget");
        // Age every failure entry past the window, as time would.
        limiter.failures.age_entries_past_window_for_test();
        assert!(
            !limiter.is_blocked(ip),
            "the block must expire with the window instead of locking the IP forever"
        );
    }

    #[test]
    fn ws_per_key_cap_releases_on_drop() {
        let limiter = std::sync::Arc::new(AdminAuthLimiter::new());
        let key = openproxy_types::ids::ApiKeyId(9);
        let guards: Vec<_> = (0..ADMIN_AUTH_MAX_WS_PER_KEY)
            .map(|_| limiter.try_acquire_ws(key).expect("slot"))
            .collect();
        assert!(limiter.try_acquire_ws(key).is_none(), "cap reached");
        drop(guards);
        assert!(limiter.try_acquire_ws(key).is_some(), "released on drop");
    }

    /// Regression: a flood of unique spoofed-IP failures MUST NOT wipe the
    /// failure counter of an already-blocked IP. Previously the overflow
    /// branch called `self.failures.clear()`, which let a blocked IP back in
    /// after the flood crested 100k entries.
    #[test]
    fn admin_auth_failure_throttle_survives_spoofed_ip_flood() {
        let limiter = std::sync::Arc::new(AdminAuthLimiter::new());
        let blocked_ip: std::net::IpAddr = "203.0.113.77".parse().unwrap();
        // Trip the throttle for `blocked_ip`.
        for _ in 0..ADMIN_AUTH_MAX_FAILURES {
            limiter.record_failure(blocked_ip);
        }
        assert!(limiter.is_blocked(blocked_ip), "blocked at budget");

        // Flood with 100_001 unique spoofed IPs (use 198.51.100.0/24 + the
        // lower 16 bits as the cycle counter to stay inside TEST-NET-2 so
        // we don't trip real outbound filtering on the test runner).
        for i in 0..100_001u32 {
            let ip: std::net::IpAddr =
                std::net::Ipv4Addr::new(198, 51, 100, (i & 0xff) as u8).into();
            limiter.record_failure(ip);
        }

        // The blocked IP MUST still be blocked after the flood: the eviction
        // path drops only expired or oldest entries, not the whole map.
        assert!(
            limiter.is_blocked(blocked_ip),
            "BLOCKED IP must remain blocked after spoofed-IP flood — clear() regression"
        );
    }
}

#[cfg(test)]
mod throttle_escape_hatch_tests {
    use super::*;
    use crate::state::AppState;

    async fn test_state() -> AppState {
        let dir = openproxy_db::testing::TempDir::new("admin-throttle-test").expect("mkdir");
        let pool = std::sync::Arc::new(
            openproxy_db::DbPool::open(&dir.join("admin-throttle.db")).expect("open"),
        );
        {
            let mut w = pool.writer();
            openproxy_db::migrations::run(&mut w).expect("migrations");
        }
        let mk = openproxy_db::MasterKey::generate().unwrap();
        let adapters = std::sync::Arc::new(parking_lot::RwLock::new(std::sync::Arc::new(
            openproxy_adapters::adapters::builtin_adapters(),
        )));
        AppState::for_test(
            openproxy_core::AppConfig::default(),
            pool,
            std::sync::Arc::new(mk),
            adapters,
        )
    }

    /// Audit H-11: a blocked IP with a CACHED valid key must pass the
    /// throttle (shared-NAT lockout), while unknown keys and headerless
    /// requests stay blocked without a database lookup.
    #[tokio::test]
    async fn blocked_ip_with_cached_valid_key_passes_throttle() {
        let state = test_state().await;
        let ip: std::net::IpAddr = "203.0.113.90".parse().unwrap();

        // Seed + cache a valid manage key.
        let plaintext = format!("op_live_admin_ok_{}", std::process::id());
        {
            let w = state.db_pool().writer();
            let key_hash = openproxy_core::api_keys::hash_key(&plaintext);
            w.execute(
                "INSERT OR REPLACE INTO api_keys (key_hash, key_prefix, label, scopes_json, created_by) \
                 VALUES (?1, ?2, 'throttle', '[\"manage\"]', 'throttle-test')",
                rusqlite::params![key_hash, &plaintext[..plaintext.len().min(12)]],
            )
            .expect("insert key");
        }
        let key = openproxy_core::api_keys::get_by_hash(
            &state.db_pool().writer(),
            &openproxy_core::api_keys::hash_key(&plaintext),
        )
        .expect("fetch key")
        .expect("key present");
        state.cache_api_key(std::sync::Arc::new(key));

        // Not blocked yet: everything passes.
        assert!(admin_throttle_allows(&state, &HeaderMap::new(), ip));

        // Trip the throttle for this IP.
        for _ in 0..ADMIN_AUTH_MAX_FAILURES {
            state.admin_limiter().record_failure(ip);
        }
        assert!(state.admin_limiter().is_blocked(ip));

        // Headerless and unknown-key requests are blocked (fast 429).
        assert!(!admin_throttle_allows(&state, &HeaderMap::new(), ip));
        let mut bad = HeaderMap::new();
        bad.insert(
            "authorization",
            "Bearer op_live_unknownkey".parse().unwrap(),
        );
        assert!(!admin_throttle_allows(&state, &bad, ip));

        // The cached valid key passes the throttle and would clear the
        // budget through record_success in the middleware.
        let mut good = HeaderMap::new();
        good.insert(
            "authorization",
            format!("Bearer {plaintext}").parse().unwrap(),
        );
        assert!(
            admin_throttle_allows(&state, &good, ip),
            "cached valid key must pass while blocked"
        );
    }
}
