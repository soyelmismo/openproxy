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
}

/// Extractor alias for handlers: the identity resolved by
/// [`admin_auth_middleware`]. `Option` because a handler may be mounted outside
/// the middleware in tests; production admin routes always have it.
pub(crate) type Identity = Option<axum::Extension<AdminIdentity>>;

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
pub(crate) fn authenticate_admin(
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
    let key = crate::middleware::auth::verify_key_credentials(state, token, "manage")?;
    Ok(AdminIdentity {
        key: Some(key),
        remote_addr: effective_remote_addr,
        client_ip,
    })
}

/// Redeem a single-use WS ticket and re-validate the key it was bound to.
fn authenticate_ws_ticket(
    state: &AppState,
    ticket: &str,
) -> Result<Arc<core_api_keys::ApiKey>, ApiError> {
    let key_id = state
        .ws_tickets()
        .consume(ticket)
        .ok_or_else(|| ApiError(CoreError::Auth("invalid or expired ws ticket".into())))?;
    let key = {
        let r = state.db_pool().reader();
        core_api_keys::get_by_id(&r, key_id)
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
pub(crate) fn authenticate_admin_ws(
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
            crate::middleware::auth::verify_key_credentials(state, token, "manage")?
        }
        (None, Some(t)) if !t.is_empty() => authenticate_ws_ticket(state, t)?,
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
    if state.admin_limiter().is_blocked(client_ip_for_throttle) {
        return crate::error::ApiError(openproxy_types::CoreError::RateLimited {
            provider: "admin_auth_throttle".into(),
            retry_after_ms: 60_000,
            is_proxy_rotated: false,
        })
        .into_response();
    }

    match authenticate_admin(&state, req.headers(), Some(&addr)) {
        Ok(identity) => {
            state.admin_limiter().record_success(client_ip_for_throttle);
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

            let existing_key =
                extract_bearer_token(req.headers())
                    .ok()
                    .flatten()
                    .and_then(|token| {
                        let key_hash = core_api_keys::hash_key(token);
                        state.get_cached_api_key(&key_hash).or_else(|| {
                            let r = state.db_pool().reader();
                            core_api_keys::get_by_hash(&r, &key_hash)
                                .ok()
                                .flatten()
                                .map(Arc::new)
                        })
                    });

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
    /// client IP -> (failures in window, window start)
    failures: dashmap::DashMap<std::net::IpAddr, (u32, std::time::Instant)>,
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
            failures: dashmap::DashMap::new(),
            ws_streams: dashmap::DashMap::new(),
        }
    }

    /// `true` when `ip` exhausted its failure budget inside the window.
    pub fn is_blocked(&self, ip: std::net::IpAddr) -> bool {
        self.failures
            .get(&ip)
            .is_some_and(|e| e.value().0 >= ADMIN_AUTH_MAX_FAILURES)
    }

    /// Count one failed attempt; resets the window when it expired.
    pub fn record_failure(&self, ip: std::net::IpAddr) {
        use dashmap::mapref::entry::Entry;
        // Bound the map under a spoofed-IP flood (matches the key cache cap).
        if self.failures.len() >= 100_000 {
            self.failures.clear();
        }
        let now = std::time::Instant::now();
        match self.failures.entry(ip) {
            Entry::Occupied(mut o) => {
                let (count, start) = o.get_mut();
                if start.elapsed() >= ADMIN_AUTH_WINDOW {
                    *count = 1;
                    *start = now;
                } else {
                    *count += 1;
                }
            }
            Entry::Vacant(v) => {
                v.insert((1, now));
            }
        }
    }

    /// A successful authentication clears the IP's failure budget.
    pub fn record_success(&self, ip: std::net::IpAddr) {
        self.failures.remove(&ip);
    }

    /// Acquire one WebSocket stream slot for `key`, or `None` when the key
    /// already holds `ADMIN_AUTH_MAX_WS_PER_KEY` live streams.
    pub fn try_acquire_ws(
        self: &std::sync::Arc<Self>,
        key: openproxy_types::ids::ApiKeyId,
    ) -> Option<WsStreamGuard> {
        use dashmap::mapref::entry::Entry;
        if self.ws_streams.len() >= 100_000 {
            self.ws_streams.clear();
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
}
