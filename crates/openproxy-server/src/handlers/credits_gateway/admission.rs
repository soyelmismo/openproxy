use super::{
    AppState, CreditsConfig, HeaderValue, IntoResponse, Next, Request, Response, State, StatusCode,
    header, json,
};

/// Per-IP admission control for the checkout surface.
///
/// Root fix for the jitter-space exhaustion DoS (audit A1-01/A4-01): every
/// order reserves one of only 10 000 live jitter slots per base price for
/// its whole window + confirmation grace, and creation was free and
/// anonymous, so ~10 000 unpaid POSTs closed the sales funnel of a price
/// (reproduced: 10 000 orders → every buyer of that price got 503) and let
/// an attacker squat the amount space. Two dedicated sliding windows bound
/// what one origin can do:
/// - **order creation** (`POST .../order`, `POST .../me/plan`):
///   `checkout_order_creations_per_ip` per `checkout_order_window_secs`
///   (default 30 / 10 min — a steady-state attacker holds ≈75 live orders,
///   0.75% of a price's slot space, vs 10 000 before);
/// - **read/reveal surface** (status poll, reveal, ack, rotate):
///   `checkout_poll_per_ip_per_min` (default 240/min vs the UI's 12/min),
///   which also bounds receipt brute-force on `/reveal`.
pub struct CheckoutAdmission {
    order: openproxy_core::rate_limit::SlidingWindowRateLimiter,
    poll: openproxy_core::rate_limit::SlidingWindowRateLimiter,
}

impl CheckoutAdmission {
    pub fn from_config(cfg: Option<&CreditsConfig>) -> Self {
        let (order_max, order_window, poll_max) = match cfg {
            Some(c) => (
                c.checkout_order_creations_per_ip.max(1),
                c.checkout_order_window_secs.max(1),
                c.checkout_poll_per_ip_per_min.max(1),
            ),
            None => (30, 600, 240),
        };
        Self {
            order: openproxy_core::rate_limit::SlidingWindowRateLimiter::new(
                openproxy_core::rate_limit::RateLimitConfig {
                    max_requests: order_max,
                    window: std::time::Duration::from_secs(order_window),
                    max_capacity: 100_000,
                },
            ),
            poll: openproxy_core::rate_limit::SlidingWindowRateLimiter::new(
                openproxy_core::rate_limit::RateLimitConfig {
                    max_requests: poll_max,
                    window: std::time::Duration::from_secs(60),
                    max_capacity: 100_000,
                },
            ),
        }
    }

    fn check_order(&self, ip: std::net::IpAddr) -> bool {
        openproxy_core::rate_limit::RateLimiter::check(
            &self.order,
            openproxy_core::rate_limit::RateLimitKey::Ip(ip),
        )
    }

    fn check_poll(&self, ip: std::net::IpAddr) -> bool {
        openproxy_core::rate_limit::RateLimiter::check(
            &self.poll,
            openproxy_core::rate_limit::RateLimitKey::Ip(ip),
        )
    }
}

/// Admission-control middleware for the whole `/v1/credits` surface. Keys on
/// the client IP (same resolution as the chat rate limiter: ConnectInfo +
/// trusted proxies). Order creation gets the strict budget; everything else
/// the poll budget. 429s carry the gateway error shape and `Retry-After`.
pub(super) async fn checkout_admission_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if state.config().credits.is_none() {
        // Gateway disabled: nothing to protect.
        return next.run(req).await;
    }
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0);
    let ip = crate::client_ip::resolve_client_ip(
        req.headers(),
        peer.as_ref(),
        &state.config().server.trusted_proxies,
    )
    .or_else(|| peer.map(|p| p.ip()))
    .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    let path = req.uri().path();
    let is_creation = req.method() == axum::http::Method::POST
        && (path.ends_with("/order") || path.ends_with("/plan"));
    let (allowed, retry_after_secs) = if is_creation {
        let window = state
            .config()
            .credits
            .as_ref()
            .map_or(600, |c| c.checkout_order_window_secs);
        (state.checkout_admission().check_order(ip), window)
    } else {
        (state.checkout_admission().check_poll(ip), 60)
    };
    if !allowed {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [
                (header::RETRY_AFTER, HeaderValue::from_str(&retry_after_secs.to_string()).unwrap_or(HeaderValue::from_static("60"))),
                (header::CONTENT_TYPE, HeaderValue::from_static("application/json")),
            ],
            json!({"error": {"code": "rate_limited", "message": "too many checkout requests from this origin; slow down and retry later"}}).to_string(),
        )
            .into_response();
    }
    next.run(req).await
}
