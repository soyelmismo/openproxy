//! ZCode activity event reporting (`https://zcode.z.ai/api/v1/event/report`).
//!
//! Reports telemetry events expected by Z.ai client protocol:
//! - `app_launch`
//! - `app_daily_active`

use super::ZAI_APP_VERSION;
use crate::adapters::{
    Arc, CancellationToken, CoreError, Result, TimeoutProfile, UpstreamClient, UpstreamRequest,
};

pub const ZAI_EVENT_REPORT_URL: &str = "https://zcode.z.ai/api/v1/event/report";
pub const EVENT_APP_LAUNCH: &str = "app_launch";
pub const EVENT_APP_DAILY_ACTIVE: &str = "app_daily_active";

/// Builds the JSON payload for a ZCode activity event.
pub fn build_activity_event_payload(
    event_type: &str,
    device_mid: &str,
    timestamp_ms: u64,
) -> serde_json::Value {
    serde_json::json!({
        "event": event_type,
        "event_type": event_type,
        "event_name": event_type,
        "app_version": ZAI_APP_VERSION,
        "device_mid": device_mid,
        "timestamp": timestamp_ms,
    })
}

/// Sends a single activity event to ZCode upstream.
pub async fn send_zai_activity_event(
    upstream: &Arc<UpstreamClient>,
    token: &str,
    event_type: &str,
    device_mid: Option<&str>,
    proxy_url: Option<&str>,
) -> Result<bool> {
    let tok = token.trim();
    if tok.is_empty() {
        return Ok(false);
    }

    let mid = device_mid
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| uuid::Uuid::new_v4().to_string(), ToString::to_string);

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);

    let payload = build_activity_event_payload(event_type, &mid, now_ms);
    let body_bytes = serde_json::to_vec(&payload)
        .map_err(|e| CoreError::Parse(format!("serialize event payload: {e}")))?;

    let mut req = UpstreamRequest::post_json(ZAI_EVENT_REPORT_URL, bytes::Bytes::from(body_bytes));
    req.proxy = proxy_url.map(ToString::to_string);

    if let Ok(val) = crate::antigravity_headers::build_bearer_header(tok) {
        req.headers.insert(http::header::AUTHORIZATION, val);
    }
    req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );
    req.headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    req.headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static("ZCode/3.14.0"),
    );
    req.headers.insert(
        http::HeaderName::from_static("x-zcode-app-version"),
        http::HeaderValue::from_static(ZAI_APP_VERSION),
    );

    if let Ok(mid_val) = http::HeaderValue::from_str(&mid) {
        req.headers
            .insert(http::HeaderName::from_static("x-device-mid"), mid_val.clone());
        req.headers
            .insert(http::HeaderName::from_static("device-mid"), mid_val);
    }

    let cancel = CancellationToken::new();
    let resp = upstream
        .call(req, TimeoutProfile::Quota, cancel)
        .await
        .map_err(|e| e.to_core_error(ZAI_EVENT_REPORT_URL))?;

    Ok(resp.status.is_success())
}

/// Reports both `app_launch` and `app_daily_active` activity events.
pub async fn report_zai_activity_events(
    upstream: &Arc<UpstreamClient>,
    token: &str,
    device_mid: Option<&str>,
    proxy_url: Option<&str>,
) -> Result<()> {
    let _ = send_zai_activity_event(upstream, token, EVENT_APP_LAUNCH, device_mid, proxy_url).await;
    let _ = send_zai_activity_event(upstream, token, EVENT_APP_DAILY_ACTIVE, device_mid, proxy_url).await;
    Ok(())
}
