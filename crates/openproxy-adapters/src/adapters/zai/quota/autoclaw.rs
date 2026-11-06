use super::dual::status_pool;
use super::envelope::envelope_is_success;
use crate::adapters::{CoreError, Result};
use openproxy_types::quota::{QuotaPool, QuotaPoolStatus, QuotaSource};
use serde_json::Value;

pub(crate) fn parse_autoclaw_pools(json: &Value, now: u64) -> Result<Vec<QuotaPool>> {
    if !envelope_is_success(json) {
        return Err(CoreError::Parse("AutoClaw wallets envelope failure".into()));
    }
    let Some(data) = json.get("data") else {
        return Ok(vec![status_pool(
            QuotaSource::AutoclawStarter,
            QuotaPoolStatus::Absent,
            None,
            now,
        )]);
    };

    let total_balance = data
        .get("total_balance")
        .and_then(Value::as_i64)
        .unwrap_or(0);

    let default_models = vec![
        "GLM-5.3-Flash".to_string(),
        "GLM-5.3".to_string(),
        "GLM-5.2".to_string(),
        "GLM-5-Turbo".to_string(),
        "glm-5.3-flash".to_string(),
        "glm-5.3".to_string(),
        "glm-5.2".to_string(),
        "glm-5-turbo".to_string(),
    ];

    if total_balance <= 0 {
        return Ok(vec![QuotaPool {
            id: "autoclaw-starter".into(),
            source: QuotaSource::AutoclawStarter,
            plan_name: Some("AutoClaw Promotional Credits".into()),
            status: QuotaPoolStatus::Exhausted,
            unit: "points".into(),
            used: None,
            limit: None,
            remaining: Some(0),
            reset_at: None,
            expires_at: None,
            starts_at: None,
            model_ids: default_models,
            model_details: None,
            fetch_error: None,
            last_fetched_at: now.to_string(),
        }]);
    }

    Ok(vec![QuotaPool {
        id: "autoclaw-starter".into(),
        source: QuotaSource::AutoclawStarter,
        plan_name: Some("AutoClaw Promotional Credits".into()),
        status: QuotaPoolStatus::Active,
        unit: "points".into(),
        used: None,
        limit: Some(total_balance),
        remaining: Some(total_balance),
        reset_at: None,
        expires_at: None,
        starts_at: None,
        model_ids: default_models,
        model_details: None,
        fetch_error: None,
        last_fetched_at: now.to_string(),
    }])
}

pub(crate) async fn perform_autoclaw_checkin(
    upstream: &std::sync::Arc<crate::upstream::UpstreamClient>,
    token: &str,
    proxy: Option<&str>,
) {
    let clean_token = token.trim();
    let raw_token = clean_token.strip_prefix("Bearer ").unwrap_or(clean_token);

    // Attempt daily_signin and daily_inspiration_center
    for task_id in ["daily_signin", "daily_inspiration_center"] {
        let app_id = "100003";
        let app_key = "6c79a4059ae078650bc7886b627cf9f9";
        let now_sec = openproxy_types::now_unix_secs_str();
        let sign = crate::adapters::zai::md5::md5::md5_hex(format!("{app_id}&{now_sec}&{app_key}"));

        let body_bytes = bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({ "task_id": task_id })).unwrap_or_default(),
        );
        let mut req = crate::upstream::UpstreamRequest::post_json(
            "https://autoglm-api.autoglm.ai/autoclaw-proxy/proxy/autoclaw-task-complete",
            body_bytes,
        );
        req.proxy = proxy.map(ToString::to_string);
        if let Ok(val) = crate::antigravity_headers::build_bearer_header(raw_token) {
            req.headers.insert(http::header::AUTHORIZATION, val);
        }
        req.headers.insert(http::header::USER_AGENT, http::HeaderValue::from_static("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36"));
        req.headers.insert(
            http::HeaderName::from_static("x-auth-appid"),
            http::HeaderValue::from_static("100003"),
        );
        if let Ok(ts_val) = http::HeaderValue::from_str(&now_sec) {
            req.headers
                .insert(http::HeaderName::from_static("x-auth-timestamp"), ts_val);
        }
        if let Ok(sign_val) = http::HeaderValue::from_str(&sign) {
            req.headers
                .insert(http::HeaderName::from_static("x-auth-sign"), sign_val);
        }
        req.headers.insert(
            http::HeaderName::from_static("x-channel"),
            http::HeaderValue::from_static("official"),
        );
        req.headers.insert(
            http::HeaderName::from_static("x-client-type"),
            http::HeaderValue::from_static("web"),
        );
        req.headers.insert(
            http::HeaderName::from_static("x-lang"),
            http::HeaderValue::from_static("es"),
        );
        req.headers.insert(
            http::HeaderName::from_static("x-product"),
            http::HeaderValue::from_static("autoclaw"),
        );
        req.headers.insert(
            http::HeaderName::from_static("x-tm"),
            http::HeaderValue::from_static("web"),
        );
        req.headers.insert(
            http::HeaderName::from_static("x-version"),
            http::HeaderValue::from_static("1.12.1"),
        );
        let trace = uuid::Uuid::new_v4().to_string();
        if let Ok(trace_val) = http::HeaderValue::from_str(&trace) {
            req.headers
                .insert(http::HeaderName::from_static("x-trace-id"), trace_val);
        }

        let _ = upstream
            .call(
                req,
                crate::upstream::TimeoutProfile::Quota,
                crate::upstream::CancellationToken::new(),
            )
            .await;
    }
}
