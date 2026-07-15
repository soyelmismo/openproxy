use super::auth::ValidatedApiToken;
use crate::{error::ApiError, state::AppState};
use axum::extract::{ConnectInfo, State};
use openproxy_types::CoreError;

pub async fn rate_limit_middleware(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, ApiError> {
    let auth_result = req.extensions().get::<ValidatedApiToken>();
    let rl_key = if let Some(t) = auth_result {
        format!("key:{}", t.key_id.0)
    } else {
        format!("ip:{}", addr.ip())
    };

    if !state.rate_limiter().check(&rl_key) {
        return Err(ApiError(CoreError::RateLimited {
            provider: "rate_limiter".into(),
            retry_after_ms: 60_000,
            is_proxy_rotated: false,
        }));
    }

    Ok(next.run(req).await)
}
