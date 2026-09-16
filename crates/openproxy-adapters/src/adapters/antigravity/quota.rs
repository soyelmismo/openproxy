use crate::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamError, UpstreamRequest,
};
use openproxy_types::{CoreError, Result};
use std::sync::Arc;

pub const NORMALIZED_BASE: i64 = 1000;

pub fn normalize_quota_fraction(
    reset_time: Option<&str>,
    raw_fraction: Option<f64>,
) -> (i64, bool) {
    let remaining_fraction = match raw_fraction {
        Some(f) if f.is_finite() => f.clamp(0.0, 1.0),
        _ => {
            if reset_time.is_some() {
                0.0
            } else {
                1.0
            }
        }
    };
    let is_unlimited = reset_time.is_none() && remaining_fraction >= 1.0;
    let remaining = (NORMALIZED_BASE as f64 * remaining_fraction) as i64;
    let used = if is_unlimited {
        0
    } else {
        NORMALIZED_BASE
            .saturating_sub(remaining)
            .clamp(0, NORMALIZED_BASE)
    };
    (used, is_unlimited)
}

pub(crate) static PLAN_CACHE: std::sync::LazyLock<
    parking_lot::RwLock<std::collections::HashMap<String, (String, std::time::Instant)>>,
> = std::sync::LazyLock::new(|| parking_lot::RwLock::new(std::collections::HashMap::new()));

pub fn prune_plan_cache() {
    let now = std::time::Instant::now();
    let max_age = std::time::Duration::from_secs(7200);
    let mut cache = PLAN_CACHE.write();
    cache.retain(|_, (_, ts)| now.duration_since(*ts) < max_age);
}

pub(crate) fn merge_summary_into_models_quota(
    models_quota: &mut openproxy_types::AccountQuota,
    summary_quota: &openproxy_types::AccountQuota,
) {
    if summary_quota.weekly_used.is_some() {
        models_quota.weekly_used = summary_quota.weekly_used;
        models_quota.weekly_limit = summary_quota.weekly_limit;
        models_quota
            .weekly_reset_at
            .clone_from(&summary_quota.weekly_reset_at);
    }
    if models_quota.session_used.is_none() && summary_quota.session_used.is_some() {
        models_quota.session_used = summary_quota.session_used;
        models_quota.session_limit = summary_quota.session_limit;
        models_quota
            .session_reset_at
            .clone_from(&summary_quota.session_reset_at);
    }
}

pub(crate) fn resolve_final_plan_name(
    models_plan: Option<String>,
    summary_res: &Result<openproxy_types::AccountQuota>,
    plan_result: Option<String>,
) -> Option<String> {
    if let Some(plan) = plan_result {
        return Some(plan);
    }
    if models_plan.is_some() && models_plan.as_deref() != Some("Antigravity") {
        return models_plan;
    }
    if let Ok(summary_quota) = summary_res
        && let Some(summary_plan) = &summary_quota.plan_name
        && summary_plan != "Antigravity"
    {
        return Some(summary_plan.clone());
    }
    Some("Free".to_string())
}

pub(crate) fn extract_tier_from_load_code_assist(json: &serde_json::Value) -> Option<&str> {
    let paid = json
        .pointer("/paidTier/name")
        .or_else(|| json.pointer("/paidTier/id"))
        .and_then(|v| v.as_str());
    if paid.is_some() {
        return paid;
    }
    let is_ineligible = json
        .pointer("/ineligibleTiers")
        .and_then(|v| v.as_array())
        .is_some_and(|a| !a.is_empty());

    if !is_ineligible {
        return json
            .pointer("/currentTier/name")
            .or_else(|| json.pointer("/currentTier/id"))
            .and_then(|v| v.as_str());
    }

    let allowed = json.pointer("/allowedTiers")?.as_array()?;
    for t in allowed {
        if t.get("isDefault")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            return t
                .get("name")
                .or_else(|| t.get("id"))
                .and_then(|v| v.as_str());
        }
    }
    None
}

const PLAN_KEYWORDS: &[(&[&str], &str)] = &[
    (&["ULTRA"], "Ultra"),
    (
        &["PRO", "PREMIUM", "GOOGLE_ONE", "ONE_AI", "GOOGLE ONE"],
        "Pro",
    ),
    (&["ENTERPRISE"], "Enterprise"),
    (&["BUSINESS", "STANDARD"], "Business"),
    (&["PLUS"], "Plus"),
    (&["LITE", "LIGHT"], "Lite"),
    (&["FREE", "INDIVIDUAL", "LEGACY"], "Free"),
];

pub(crate) fn classify_antigravity_plan_name(t: &str) -> String {
    let upper = t.to_uppercase();
    for (keywords, plan) in PLAN_KEYWORDS {
        if keywords.iter().any(|k| upper.contains(k)) {
            return (*plan).to_string();
        }
    }
    t.to_string()
}

pub(crate) fn parse_model_quota_detail(
    model_id: &str,
    model_data: &serde_json::Value,
) -> Option<openproxy_types::ModelQuotaDetail> {
    let quota_info = model_data.get("quotaInfo")?;
    let reset_time = quota_info
        .get("resetTime")
        .and_then(|r| r.as_str())
        .map(String::from);
    let raw_fraction = quota_info
        .get("remainingFraction")
        .and_then(serde_json::Value::as_f64);
    let (used, _) = normalize_quota_fraction(reset_time.as_deref(), raw_fraction);
    let remaining_fraction =
        raw_fraction.unwrap_or_else(|| if reset_time.is_some() { 0.0 } else { 1.0 });

    Some(openproxy_types::ModelQuotaDetail {
        model_id: model_id.to_string(),
        session_used: used,
        session_limit: NORMALIZED_BASE,
        session_reset_at: reset_time,
        remaining_fraction,
    })
}

pub fn parse_antigravity_models_response(
    body: &serde_json::Value,
) -> Result<openproxy_types::AccountQuota> {
    let models = body
        .get("models")
        .and_then(|m| m.as_object())
        .ok_or_else(|| CoreError::Internal("missing 'models' in response".into()))?;

    let details: Vec<openproxy_types::ModelQuotaDetail> = models
        .iter()
        .filter_map(|(k, v)| parse_model_quota_detail(k, v))
        .collect();

    let worst = details
        .iter()
        .min_by(|a, b| {
            a.remaining_fraction
                .partial_cmp(&b.remaining_fraction)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .ok_or_else(|| CoreError::Internal("no quota info found in response".into()))?;

    Ok(openproxy_types::AccountQuota {
        plan_name: Some("Antigravity".to_string()),
        session_used: Some(worst.session_used),
        session_limit: Some(worst.session_limit),
        session_reset_at: worst.session_reset_at.clone(),
        weekly_used: None,
        weekly_limit: None,
        weekly_reset_at: None,
        last_fetched_at: openproxy_types::now_unix_secs_str(),
        fetch_error: None,
        model_details: Some(details.into()),
    })
}

pub(crate) struct AntigravityQuotaBucket {
    pub(crate) plan_name: Option<String>,
    pub(crate) is_weekly: bool,
    pub(crate) used: i64,
    pub(crate) reset_at: Option<String>,
}

pub(crate) fn parse_quota_bucket(
    group_plan: Option<&str>,
    bucket: &serde_json::Value,
) -> AntigravityQuotaBucket {
    let reset_time = bucket
        .get("resetTime")
        .and_then(|r| r.as_str())
        .map(String::from);
    let window = bucket.get("window").and_then(|w| w.as_str()).unwrap_or("");
    let raw_fraction = bucket
        .get("remainingFraction")
        .and_then(serde_json::Value::as_f64);
    let (used, _) = normalize_quota_fraction(reset_time.as_deref(), raw_fraction);
    let is_weekly = window.to_uppercase().contains("WEEK") || window.eq_ignore_ascii_case("WEEKLY");

    AntigravityQuotaBucket {
        plan_name: group_plan.map(std::string::ToString::to_string),
        is_weekly,
        used,
        reset_at: reset_time,
    }
}

pub(crate) fn extract_quota_buckets(
    groups: &[serde_json::Value],
) -> impl Iterator<Item = AntigravityQuotaBucket> + '_ {
    groups.iter().flat_map(|group| {
        let group_plan = group.get("displayName").and_then(|n| n.as_str());
        let buckets = group
            .get("buckets")
            .and_then(|b| b.as_array())
            .map_or(&[][..], |v| v.as_slice());

        buckets
            .iter()
            .map(move |bucket| parse_quota_bucket(group_plan, bucket))
    })
}

pub fn parse_antigravity_user_quota_summary(
    body: &serde_json::Value,
) -> Result<openproxy_types::AccountQuota> {
    let groups = body
        .get("groups")
        .and_then(|g| g.as_array())
        .ok_or_else(|| {
            CoreError::Internal("missing 'groups' in retrieveUserQuotaSummary".into())
        })?;

    let mut weekly_used = None;
    let mut weekly_limit = None;
    let mut weekly_reset_at = None;
    let mut session_used = None;
    let mut session_limit = None;
    let mut session_reset_at = None;
    let mut plan_name = None;

    for bucket in extract_quota_buckets(groups) {
        if bucket.is_weekly {
            if weekly_used.is_none() {
                weekly_used = Some(bucket.used);
                weekly_limit = Some(NORMALIZED_BASE);
                weekly_reset_at = bucket.reset_at;
                plan_name = plan_name.or(bucket.plan_name);
            }
        } else if session_used.is_none() {
            session_used = Some(bucket.used);
            session_limit = Some(NORMALIZED_BASE);
            session_reset_at = bucket.reset_at;
            plan_name = plan_name.or(bucket.plan_name);
        }
    }

    if weekly_used.is_none() && session_used.is_none() {
        return Err(CoreError::Internal(
            "retrieveUserQuotaSummary: no usable buckets found".into(),
        ));
    }

    Ok(openproxy_types::AccountQuota {
        session_used,
        session_limit,
        session_reset_at,
        weekly_used,
        weekly_limit,
        weekly_reset_at,
        plan_name: Some(plan_name.unwrap_or_else(|| "Antigravity".to_string())),
        last_fetched_at: openproxy_types::now_unix_secs_str(),
        fetch_error: None,
        model_details: None,
    })
}

pub async fn fetch_antigravity_quota_local(
    upstream: &Arc<UpstreamClient>,
    access_token: &str,
) -> Result<openproxy_types::AccountQuota> {
    let (models_result, summary_result, plan_result) = tokio::join!(
        fetch_antigravity_models_quota_local(upstream, access_token),
        fetch_antigravity_user_quota_local(upstream, access_token),
        fetch_antigravity_subscription_plan_local(upstream, access_token),
    );

    match (models_result, summary_result) {
        (Ok(mut models_quota), summary_res) => {
            if let Ok(summary_quota) = &summary_res {
                merge_summary_into_models_quota(&mut models_quota, summary_quota);
            }
            models_quota.plan_name =
                resolve_final_plan_name(models_quota.plan_name, &summary_res, plan_result);
            Ok(models_quota)
        }
        (Err(_models_err), Ok(mut summary_quota)) => {
            let current_plan = summary_quota.plan_name.clone();
            summary_quota.plan_name =
                resolve_final_plan_name(current_plan, &Ok(summary_quota.clone()), plan_result);
            Ok(summary_quota)
        }
        (Err(models_err), Err(_)) => Err(models_err),
    }
}

pub async fn fetch_antigravity_user_quota_local(
    upstream: &Arc<UpstreamClient>,
    access_token: &str,
) -> Result<openproxy_types::AccountQuota> {
    let endpoints = [
        "https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal:retrieveUserQuotaSummary",
        "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary",
        "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary",
    ];

    let mut last_err: Option<CoreError> = None;
    for url in &endpoints {
        let mut req = UpstreamRequest::post_json(*url, bytes::Bytes::from_static(b"{}"));
        if let Err(e) = crate::antigravity_headers::insert_bearer(&mut req, access_token) {
            last_err = Some(CoreError::UpstreamConnection(format!(
                "{url}: invalid bearer token: {e}"
            )));
            continue;
        }
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        crate::antigravity_headers::inject_antigravity_headers(&mut req.headers, None);

        let cancel = CancellationToken::new();
        let response = match upstream.call(req, TimeoutProfile::Quota, cancel).await {
            Ok(r) => r,
            Err(UpstreamError::Cancel) => {
                return Err(CoreError::Cancelled(
                    openproxy_types::CancelReason::ClientDisconnected,
                ));
            }
            Err(e) => {
                last_err = Some(CoreError::UpstreamConnection(e.to_string()));
                continue;
            }
        };

        if !response.status.is_success() {
            last_err = Some(CoreError::UpstreamConnection(format!(
                "{url}: status {}",
                response.status.as_u16()
            )));
            continue;
        }

        let body = match response.collect().await {
            Ok(b) => b,
            Err(e) => {
                last_err = Some(CoreError::UpstreamConnection(e.to_string()));
                continue;
            }
        };

        let json: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(j) => j,
            Err(e) => {
                last_err = Some(CoreError::Parse(e.to_string()));
                continue;
            }
        };

        match parse_antigravity_user_quota_summary(&json) {
            Ok(q) => return Ok(q),
            Err(e) => {
                last_err = Some(e);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| {
        CoreError::UpstreamConnection("all retrieveUserQuotaSummary endpoints failed".into())
    }))
}

pub async fn fetch_antigravity_models_quota_local(
    upstream: &Arc<UpstreamClient>,
    access_token: &str,
) -> Result<openproxy_types::AccountQuota> {
    let endpoints = [
        "https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels",
        "https://cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels",
    ];
    let json: serde_json::Value = crate::antigravity_headers::fetch_with_fallback(
        upstream,
        &endpoints,
        &serde_json::json!({}),
        access_token,
        TimeoutProfile::Quota,
        "antigravity fetchAvailableModels quota",
    )
    .await
    .map_err(CoreError::UpstreamConnection)?;
    parse_antigravity_models_response(&json)
}

pub async fn fetch_antigravity_subscription_plan_local(
    upstream: &Arc<UpstreamClient>,
    access_token: &str,
) -> Option<String> {
    let now = std::time::Instant::now();
    if let Some((plan, ts)) = PLAN_CACHE.read().get(access_token)
        && now.duration_since(*ts) < std::time::Duration::from_secs(7200)
    {
        return Some(plan.clone());
    }

    let endpoints = [
        "https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal:loadCodeAssist",
        "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist",
    ];

    let payload = serde_json::json!({ "metadata": { "ideType": "ANTIGRAVITY" } });

    let json: serde_json::Value = crate::antigravity_headers::fetch_with_fallback(
        upstream,
        &endpoints,
        &payload,
        access_token,
        TimeoutProfile::Quota,
        "antigravity loadCodeAssist",
    )
    .await
    .ok()?;

    let tier = extract_tier_from_load_code_assist(&json)?;
    let plan = classify_antigravity_plan_name(tier);
    PLAN_CACHE
        .write()
        .insert(access_token.to_string(), (plan.clone(), now));
    Some(plan)
}
