use super::auth::ValidatedApiToken;
use crate::{error::ApiError, state::AppState};
use axum::extract::State;
use openproxy_core::rate_limit::RateLimitKey;
use openproxy_types::CoreError;

/// Response body wrapper that holds one in-flight slot until the body has
/// fully finished streaming.
///
/// Security (OP-03): the middleware future resolves as soon as response
/// headers are ready, which for SSE is long before the stream ends. Tying the
/// release to the body — not to the middleware — is what actually bounds the
/// number of concurrent 300 s streams a single key can hold.
struct InFlightBody {
    inner: axum::body::Body,
    _guard: openproxy_core::rate_limit::InFlightGuard,
}

impl http_body::Body for InFlightBody {
    type Data = bytes::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        std::pin::Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

pub async fn rate_limit_middleware(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, ApiError> {
    // The peer address is read from the request extensions when present so the
    // middleware also works in `oneshot` tests that do not attach
    // `ConnectInfo` (extracting `ConnectInfo<SocketAddr>` directly would turn
    // those tests into 500s).
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0);

    let auth_result = req.extensions().get::<ValidatedApiToken>();
    let rl_key = if let Some(t) = auth_result {
        RateLimitKey::Key(t.key_id)
    } else {
        let client_ip = crate::client_ip::resolve_client_ip(
            req.headers(),
            peer.as_ref(),
            &state.config().server.trusted_proxies,
        )
        .or_else(|| peer.map(|p| p.ip()))
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
        RateLimitKey::Ip(client_ip)
    };

    if !state.rate_limiter().check(rl_key) {
        return Err(ApiError(CoreError::RateLimited {
            provider: "rate_limiter".into(),
            retry_after_ms: 60_000,
            is_proxy_rotated: false,
        }));
    }

    // Concurrency caps (OP-03): a proxy-wide ceiling plus a small per-key
    // fairness bound, so one client cannot monopolise the global pool.
    let Some(guard) = state.inflight_limiter().try_acquire(rl_key) else {
        // Report which ceiling refused the slot: a per-key refusal means
        // "this client is too busy", a global one means "the proxy is full".
        let per_key_full =
            state.inflight_limiter().active(rl_key) >= state.max_concurrent_per_key();
        let provider = if per_key_full {
            "concurrency_limiter_per_key"
        } else {
            "concurrency_limiter_global"
        };
        return Err(ApiError(CoreError::RateLimited {
            provider: provider.into(),
            retry_after_ms: 1_000,
            is_proxy_rotated: false,
        }));
    };

    let resp = next.run(req).await;
    let (parts, body) = resp.into_parts();
    Ok(axum::response::Response::from_parts(
        parts,
        axum::body::Body::new(InFlightBody {
            inner: body,
            _guard: guard,
        }),
    ))
}
