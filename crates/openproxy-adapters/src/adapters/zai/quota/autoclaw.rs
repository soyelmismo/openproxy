use super::dual::status_pool;
use super::envelope::envelope_is_success;
use crate::adapters::{CoreError, Result};
use openproxy_types::quota::{QuotaPool, QuotaPoolStatus, QuotaSource};
use serde_json::Value;

pub(crate) fn parse_autoclaw_pools(json: &Value, now: u64) -> Result<Vec<QuotaPool>> {
    if !envelope_is_success(json) {
        return Err(CoreError::Parse("AutoClaw wallets envelope failure".into()));
    }
    let data = match json.get("data") {
        Some(d) if d.is_object() => d,
        Some(d) if d.is_null() => {
            return Err(CoreError::Parse(
                "AutoClaw wallets response carried null data".into(),
            ));
        }
        None => {
            return Ok(vec![status_pool(
                QuotaSource::AutoclawStarter,
                QuotaPoolStatus::Absent,
                None,
                now,
            )]);
        }
        Some(_) => {
            return Err(CoreError::Parse(
                "AutoClaw wallets response data is not an object".into(),
            ));
        }
    };

    let Some(total_balance) = super::envelope::as_i64(data.get("total_balance")) else {
        return Err(CoreError::Parse(
            "AutoClaw wallets response carried no valid total_balance".into(),
        ));
    };

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

const CHECKIN_MIN_INTERVAL_SECS: u64 = 6 * 3600;

fn checkin_throttle_allows(token: &str, now_secs: u64) -> bool {
    use std::collections::HashMap;
    use std::hash::{Hash, Hasher};
    static LAST: std::sync::OnceLock<std::sync::Mutex<HashMap<u64, u64>>> =
        std::sync::OnceLock::new();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    token.hash(&mut hasher);
    let key = hasher.finish();
    let map = LAST.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
    match guard.get(&key) {
        Some(last) if now_secs.saturating_sub(*last) < CHECKIN_MIN_INTERVAL_SECS => false,
        _ => {
            guard.insert(key, now_secs);
            true
        }
    }
}

pub struct AutoclawCheckinResult {
    pub last_checkin_date: String,
    pub streak_days: u32,
}

pub async fn perform_autoclaw_checkin(
    upstream: &std::sync::Arc<crate::upstream::UpstreamClient>,
    token: &str,
    proxy: Option<&str>,
) -> Option<AutoclawCheckinResult> {
    let now_u64 = openproxy_types::now_unix_secs_str()
        .parse::<u64>()
        .unwrap_or(0);
    if !checkin_throttle_allows(token, now_u64) {
        return None;
    }
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

    // Query task list to retrieve current streak and checkin status
    let app_id = "100003";
    let app_key = "6c79a4059ae078650bc7886b627cf9f9";
    let now_sec = openproxy_types::now_unix_secs_str();
    let sign = crate::adapters::zai::md5::md5::md5_hex(format!("{app_id}&{now_sec}&{app_key}"));

    let mut list_req = crate::upstream::UpstreamRequest::get(
        "https://autoglm-api.autoglm.ai/autoclaw-proxy/proxy/autoclaw-task-list",
    );
    list_req.proxy = proxy.map(ToString::to_string);
    if let Ok(val) = crate::antigravity_headers::build_bearer_header(raw_token) {
        list_req.headers.insert(http::header::AUTHORIZATION, val);
    }
    list_req.headers.insert(http::header::USER_AGENT, http::HeaderValue::from_static("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36"));
    list_req.headers.insert(
        http::HeaderName::from_static("x-auth-appid"),
        http::HeaderValue::from_static("100003"),
    );
    if let Ok(ts_val) = http::HeaderValue::from_str(&now_sec) {
        list_req
            .headers
            .insert(http::HeaderName::from_static("x-auth-timestamp"), ts_val);
    }
    if let Ok(sign_val) = http::HeaderValue::from_str(&sign) {
        list_req
            .headers
            .insert(http::HeaderName::from_static("x-auth-sign"), sign_val);
    }
    list_req.headers.insert(
        http::HeaderName::from_static("x-channel"),
        http::HeaderValue::from_static("official"),
    );
    list_req.headers.insert(
        http::HeaderName::from_static("x-client-type"),
        http::HeaderValue::from_static("web"),
    );
    list_req.headers.insert(
        http::HeaderName::from_static("x-lang"),
        http::HeaderValue::from_static("es"),
    );
    list_req.headers.insert(
        http::HeaderName::from_static("x-product"),
        http::HeaderValue::from_static("autoclaw"),
    );
    list_req.headers.insert(
        http::HeaderName::from_static("x-tm"),
        http::HeaderValue::from_static("web"),
    );
    list_req.headers.insert(
        http::HeaderName::from_static("x-version"),
        http::HeaderValue::from_static("1.12.1"),
    );

    if let Ok(resp) = upstream
        .call(
            list_req,
            crate::upstream::TimeoutProfile::Quota,
            crate::upstream::CancellationToken::new(),
        )
        .await
        && resp.status.is_success()
        && let Ok(body_bytes) = resp.collect().await
        && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body_bytes)
        && let Some(tasks) = v.get("data").and_then(|d| d.as_array())
    {
        for t in tasks {
            if t.get("task_id").and_then(|id| id.as_str()) == Some("daily_signin") {
                let mut streak = 0;
                if let Some(desc) = t.get("status_description").and_then(|d| d.as_str()) {
                    for word in desc.split(|c: char| !c.is_ascii_digit()) {
                        if let Ok(n) = word.parse::<u32>() {
                            streak = n;
                            break;
                        }
                    }
                }
                let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
                return Some(AutoclawCheckinResult {
                    last_checkin_date: today,
                    streak_days: streak,
                });
            }
        }
    }

    None
}
