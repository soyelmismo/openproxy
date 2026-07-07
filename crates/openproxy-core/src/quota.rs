//! Per-account quota fetchers.
//!
//! Each provider that exposes a quota endpoint gets its own fetcher that
//! knows the URL, the request shape, and the response parser. The
//! `AccountQuota` struct is the common wire shape we stamp onto
//! `accounts` (see migration 000012).
//!
//! ## Quota-capable providers (MVP)
//!
//! - MiniMax (`provider_id == "minimax"` and `"minimax-cn"`) — tries the
//!   `token_plan/remains` endpoint first, then `coding_plan/remains`.
//!
//! Other providers (OpenRouter, OpenCode Zen, custom providers) have no
//! quota endpoint to call, so their account rows always have NULL
//! `quota_*` columns and the UI shows "not supported by provider".

use crate::error::{CoreError, Result};
use crate::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamError, UpstreamRequest,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Per-model quota detail. Returned inside `AccountQuota::model_details`
/// for providers that expose per-model quota (Antigravity family).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelQuotaDetail {
    pub model_id: String,
    pub session_used: i64,
    pub session_limit: i64,
    pub session_reset_at: Option<String>,
    pub remaining_fraction: f64,
}

/// Quota snapshot for a single account.
///
/// All numeric fields are `Option<i64>` because the upstream may omit
/// them (e.g. an account with no rate limit). `last_fetched_at` is the
/// so the UI can show "fetched 12 min ago" and so the operator can
/// spot a stuck fetcher (a successful fetch updates the timestamp even
/// when the body is empty).
///
/// `fetch_error` carries a short error message when the upstream
/// returned a non-2xx or the body failed to parse. The other fields
/// may still be populated in that case (partial parse), or all-NULL
/// if nothing was recoverable. The UI treats `fetch_error != None` as
/// the "this account's quota is unknown" signal and shows the message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountQuota {
    pub session_used: Option<i64>,
    pub session_limit: Option<i64>,
    pub session_reset_at: Option<String>, // ISO-8601 or epoch seconds
    pub weekly_used: Option<i64>,
    pub weekly_limit: Option<i64>,
    pub weekly_reset_at: Option<String>,
    pub plan_name: Option<String>,
    pub last_fetched_at: String,
    pub fetch_error: Option<String>,
    /// Per-model quota breakdown (Antigravity family providers).
    /// When present, the UI renders a model-by-model list below the
    /// aggregate progress bars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_details: Option<Vec<ModelQuotaDetail>>,
}

impl AccountQuota {
    /// True when no useful data is present: no usage counters, no
    /// reset times, and no error message. The UI uses this to decide
    /// between "not fetched yet" (no rows ever written) and "fetched,
    /// but the upstream said nothing".
    pub fn is_empty(&self) -> bool {
        self.session_used.is_none() && self.weekly_used.is_none() && self.fetch_error.is_none()
    }
}

/// Fetch MiniMax Coding Plan quota.
///
/// Tries `https://api.minimax.io/v1/token_plan/remains` first; on any
/// non-success response it falls back to
/// `https://api.minimax.io/v1/api/openplatform/coding_plan/remains`.
/// If both fail, the returned `AccountQuota` is `is_empty()` plus a
/// `fetch_error` describing the last failure. A successful parse from
/// either endpoint sets the `plan_name` and the session/weekly
/// counters (when present in the body).
///
/// `api_key` is the plaintext API key — the caller is responsible for
/// decrypting it from the row. The function holds the key only for the
/// lifetime of the HTTP call.
pub async fn fetch_minimax_quota(
    upstream: &Arc<UpstreamClient>,
    api_key: &str,
) -> Result<AccountQuota> {
    let urls = [
        "https://api.minimax.io/v1/token_plan/remains",
        "https://api.minimax.io/v1/api/openplatform/coding_plan/remains",
    ];

    let mut last_err: Option<String> = None;
    for url in &urls {
        match fetch_minimax_from_url(upstream, api_key, url).await {
            Ok(quota) => return Ok(quota),
            Err(e) => last_err = Some(format!("{}: {}", url, e)),
        }
    }

    // Both endpoints failed; record the most recent error so the UI can
    // surface it. Numeric fields stay None.
    Ok(AccountQuota {
        session_used: None,
        session_limit: None,
        session_reset_at: None,
        weekly_used: None,
        weekly_limit: None,
        weekly_reset_at: None,
        plan_name: None,
        last_fetched_at: now_unix_secs_str(),
        fetch_error: Some(last_err.unwrap_or_else(|| "unknown error".into())),
        model_details: None,
    })
}

/// GET a single MiniMax quota endpoint, returning either a parsed
/// `AccountQuota` or a `CoreError` describing the failure. Used
/// internally by [`fetch_minimax_quota`]'s fallback loop.
async fn fetch_minimax_from_url(
    upstream: &Arc<UpstreamClient>,
    api_key: &str,
    url: &str,
) -> Result<AccountQuota> {
    let mut req = UpstreamRequest::get(url);
    if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {api_key}")) {
        req.headers.insert(http::header::AUTHORIZATION, v);
    }
    let cancel = CancellationToken::new();
    let response = upstream
        .call(req, TimeoutProfile::Quota, cancel)
        .await
        .map_err(|e| match e {
            UpstreamError::Cancel => CoreError::ClientDisconnected,
            other => CoreError::UpstreamConnection(format!("{}: {}", url, other)),
        })?;

    if !response.status.is_success() {
        return Err(CoreError::UpstreamConnection(format!(
            "{}: status {}",
            url,
            response.status.as_u16()
        )));
    }

    let body = response
        .collect()
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("{}: {}", url, e)))?;

    let json: serde_json::Value =
        serde_json::from_slice(&body).map_err(|e| CoreError::Parse(format!("{}: {}", url, e)))?;
    parse_minimax_quota(&json, url)
}

/// Parse the JSON body MiniMax returns from its quota endpoints.
///
/// The upstream has shipped two response shapes over time, so the
/// parser accepts both and picks the most informative one per
/// (session, weekly) pair.
///
/// **Shape A — counts** (token_plan endpoint, older coding_plan rows):
/// ```json
/// {
///   "plan_name": "Coding Plan",
///   "model_remains": [
///     {
///       "model_name": "general",
///       "current_interval_usage_count": 123,
///       "current_interval_total_count": 5000,
///       "remains_time": 1700000000000,
///       "current_weekly_usage_count": 8000,
///       "current_weekly_total_count": 50000,
///       "weekly_remains_time": 1700000000000
///     }
///   ]
/// }
/// ```
///
/// **Shape B — percent only** (current coding_plan response, per
/// model_name "general"): the upstream returns only the share of the
/// window that is still available, not the absolute counts.
/// ```json
/// {
///   "plan_name": "Coding Plan Lite",
///   "model_remains": [
///     {
///       "model_name": "general",
///       "current_interval_remaining_percent": 25,
///       "current_weekly_remaining_percent": 50,
///       "remains_time": 1700000000000,
///       "weekly_remains_time": 1800000000000
///     }
///   ]
/// }
/// ```
/// We translate "25% remaining" → `session_used=75, session_limit=100`
/// so the UI can keep rendering the same bar. The 100 ceiling is
/// synthetic — the UI is expected to label the bar as a percentage
/// rather than a raw count when the limit equals 100.
///
/// Counts take precedence over percent when both are present (some
/// per-model rows still ship counts even when the `general` row
/// reports percent). When a row ships neither, the field stays `None`
/// and the UI shows "—".
///
/// We pick the entry whose `model_name` is `general` (Coding Plan's
/// generic tier), or `coding-plan`, or any `MiniMax-M*` model — falling
/// back to the first entry when none match.
fn parse_minimax_quota(body: &serde_json::Value, url: &str) -> Result<AccountQuota> {
    let plan_name = body
        .get("plan_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let entries = body
        .get("model_remains")
        .and_then(|v| v.as_array())
        .ok_or_else(|| CoreError::Parse(format!("{}: missing 'model_remains' array", url)))?;

    if entries.is_empty() {
        return Err(CoreError::Parse(format!("{}: empty model_remains", url)));
    }

    let target = entries
        .iter()
        .find(|e| {
            let name = e.get("model_name").and_then(|v| v.as_str()).unwrap_or("");
            let lower = name.to_ascii_lowercase();
            // Prefer the Coding Plan aggregate tier over a per-model
            // row. The upstream returns one `model_remains` entry per
            // available model (`MiniMax-M2.5`, `MiniMax-M2.7`, …) plus
            // an aggregate `general` row that sums usage across the
            // whole Coding Plan. The aggregate is what the user wants
            // to see in the dashboard.
            lower == "general" || lower == "coding-plan"
        })
        .or_else(|| {
            // No aggregate present? Fall back to the first per-model
            // row that looks like a MiniMax-M* model. This keeps the
            // quota informative when the upstream drops the `general`
            // entry but still returns a known-good per-model row.
            entries.iter().find(|e| {
                let name = e.get("model_name").and_then(|v| v.as_str()).unwrap_or("");
                name.to_ascii_lowercase().starts_with("minimax-m")
            })
        })
        .or_else(|| entries.first())
        .expect("non-empty checked above");

    let (session_used, session_limit) = extract_used_limit(
        target,
        "current_interval_usage_count",
        "current_interval_total_count",
        "current_interval_remaining_percent",
    );
    let (weekly_used, weekly_limit) = extract_used_limit(
        target,
        "current_weekly_usage_count",
        "current_weekly_total_count",
        "current_weekly_remaining_percent",
    );

    // Reset times: MiniMax returns milliseconds since the unix epoch.
    // We pass them through as a string of the epoch seconds — keeps
    // the column TEXT and lets the JS side format it for display.
    let session_reset_at = target
        .get("remains_time")
        .and_then(|v| v.as_i64())
        .and_then(ms_epoch_to_secs_str);
    let weekly_reset_at = target
        .get("weekly_remains_time")
        .and_then(|v| v.as_i64())
        .and_then(ms_epoch_to_secs_str);

    Ok(AccountQuota {
        session_used,
        session_limit,
        session_reset_at,
        weekly_used,
        weekly_limit,
        weekly_reset_at,
        plan_name,
        last_fetched_at: now_unix_secs_str(),
        fetch_error: None,
        model_details: None,
    })
}

/// Resolve the (used, limit) pair for a single quota window from one
/// `model_remains` entry.
///
/// Tries the absolute-count fields first (`*_usage_count` /
/// `*_total_count`); when those are absent, falls back to the
/// remaining-percent field (`*_remaining_percent`) reported by the
/// current MiniMax coding_plan endpoint. The percent branch
/// synthesises a 0-100 pair — see [`parse_minimax_quota`]'s doc
/// comment for the rationale.
fn extract_used_limit(
    entry: &serde_json::Value,
    used_count_key: &str,
    limit_count_key: &str,
    remaining_pct_key: &str,
) -> (Option<i64>, Option<i64>) {
    // 1. Counts: both fields present, with a positive limit (a limit
    //    of 0 would divide-by-zero downstream and isn't a meaningful
    //    quota anyway — treat as missing).
    let used = entry.get(used_count_key).and_then(|v| v.as_i64());
    let limit = entry.get(limit_count_key).and_then(|v| v.as_i64());
    if let (Some(u), Some(l)) = (used, limit)
        && l > 0
    {
        return (Some(u), Some(l));
    }

    // 2. Percent: the upstream exposes "remaining" (e.g. 25 = 25%
    //    of the window left), not "used". Invert it and pin the
    //    limit at 100 so the bar math is unchanged. Negative inputs
    //    or values above 100 are rejected — the upstream shouldn't
    //    send them, and trusting them would let a bug paint a 200%
    //    bar.
    let remaining = entry.get(remaining_pct_key).and_then(|v| v.as_i64());
    if let Some(rp) = remaining
        && (0..=100).contains(&rp)
    {
        let used_calc = (100 - rp).max(0);
        return (Some(used_calc), Some(100));
    }

    (None, None)
}

/// Convert a unix-epoch milliseconds value to a seconds-since-epoch
/// decimal string. We strip sub-second precision because the UI only
/// formats it as a wall-clock timestamp and a float is harder to
/// reason about across the JS/Rust boundary.
fn ms_epoch_to_secs_str(ms: i64) -> Option<String> {
    let secs = ms.checked_div(1000)?;
    Some(secs.to_string())
}

/// Current wall-clock time as a string of seconds since the unix
/// epoch. The string format is intentional: the column is TEXT, and
/// the JS side parses it as `parseInt` to format it as a relative
/// timestamp ("3m ago") or absolute.
fn now_unix_secs_str() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
}

// =====================================================================
// Antigravity (Google Cloud Code)
// =====================================================================

/// Fetch quota for Antigravity (Google Cloud Code) using an OAuth access
/// token. Calls BOTH endpoints and merges the results:
///   - `fetchAvailableModels` → per-model quota (`model_details`)
///   - `retrieveUserQuotaSummary` → weekly + 5h grouped quota
///
/// If one endpoint fails, the other's data is still returned (partial
/// success). If both fail, the error from the first is returned.
///
/// `access_token` is the *plaintext* OAuth access token — the caller
/// is responsible for decrypting it from the account row.
pub async fn fetch_antigravity_quota(
    upstream: &Arc<UpstreamClient>,
    access_token: &str,
) -> Result<AccountQuota> {
    let models_result = fetch_antigravity_models_quota(upstream, access_token).await;
    let summary_result = fetch_antigravity_user_quota(upstream, access_token).await;

    match (models_result, summary_result) {
        (Ok(mut models_quota), Ok(summary_quota)) => {
            // Merge: models_quota has per-model details; summary_quota
            // has weekly + 5h grouped quota. Overlay the summary's
            // weekly fields onto the models quota.
            if summary_quota.weekly_used.is_some() {
                models_quota.weekly_used = summary_quota.weekly_used;
                models_quota.weekly_limit = summary_quota.weekly_limit;
                models_quota.weekly_reset_at = summary_quota.weekly_reset_at;
            }
            // If models_quota has no session data, use the summary's
            // 5h (session) data as a fallback.
            if models_quota.session_used.is_none() && summary_quota.session_used.is_some() {
                models_quota.session_used = summary_quota.session_used;
                models_quota.session_limit = summary_quota.session_limit;
                models_quota.session_reset_at = summary_quota.session_reset_at;
            }
            
            if let Some(summary_plan) = summary_quota.plan_name 
                && summary_plan != "Antigravity" 
            {
                models_quota.plan_name = Some(summary_plan);
            }
            
            Ok(models_quota)
        }
        (Ok(models_quota), Err(_)) => Ok(models_quota),
        (Err(_), Ok(summary_quota)) => Ok(summary_quota),
        (Err(models_err), Err(_)) => Err(models_err),
    }
}

/// Fetch quota from the `fetchAvailableModels` endpoint.
async fn fetch_antigravity_models_quota(
    upstream: &Arc<UpstreamClient>,
    access_token: &str,
) -> Result<AccountQuota> {
    let endpoints = [
        "https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels",
        "https://cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels",
    ];

    for endpoint in &endpoints {
        let mut req = UpstreamRequest::post_json(*endpoint, bytes::Bytes::from_static(b"{}"));
        if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {access_token}")) {
            req.headers.insert(http::header::AUTHORIZATION, v);
        }
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        // CRITICAL: inject the full set of Antigravity client-identity
        // headers (User-Agent, x-client-name, x-client-version,
        // x-machine-id, x-vscode-sessionid). The previous code only
        // sent a hardcoded User-Agent and x-goog-api-client — the API
        // may reject requests missing the x-client-name / x-machine-id
        // headers. See `antigravity_headers` module.
        crate::antigravity_headers::inject_antigravity_headers(&mut req.headers, None);

        let cancel = CancellationToken::new();
        let response = upstream.call(req, TimeoutProfile::Quota, cancel).await;

        if let Ok(resp) = response
            && resp.status.is_success()
        {
            let body = match resp.collect().await {
                Ok(b) => b,
                Err(_) => continue,
            };
            if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body) {
                return parse_antigravity_models_response(&json);
            }
        }
    }

    Err(CoreError::UpstreamConnection(
        "all fetchAvailableModels endpoints failed".into(),
    ))
}

/// Parse `fetchAvailableModels` response into `AccountQuota`.
///
/// Collects per-model quota from ALL models into `model_details` and
/// uses the model with the lowest remaining fraction as the aggregate
/// (worst-case) indicator.
fn parse_antigravity_models_response(body: &serde_json::Value) -> Result<AccountQuota> {
    const NORMALIZED_BASE: i64 = 1000;

    let models = body
        .get("models")
        .and_then(|m| m.as_object())
        .ok_or_else(|| CoreError::Internal("missing 'models' in response".into()))?;

    let mut details: Vec<ModelQuotaDetail> = Vec::new();
    let mut worst_remaining = f64::MAX;
    let mut worst_model_id = String::new();

    for (model_id, model_data) in models {
        let Some(quota_info) = model_data.get("quotaInfo") else {
            continue;
        };
        let reset_time = quota_info
            .get("resetTime")
            .and_then(|r| r.as_str())
            .map(String::from);

        let remaining_fraction = quota_info
            .get("remainingFraction")
            .and_then(|f| f.as_f64())
            .unwrap_or_else(|| if reset_time.is_some() { 0.0 } else { 1.0 });

        let is_unlimited = reset_time.is_none() && remaining_fraction >= 1.0;
        let remaining = (NORMALIZED_BASE as f64 * remaining_fraction) as i64;
        let used = if is_unlimited {
            0
        } else {
            NORMALIZED_BASE.saturating_sub(remaining)
        };

        details.push(ModelQuotaDetail {
            model_id: model_id.clone(),
            session_used: used,
            session_limit: NORMALIZED_BASE,
            session_reset_at: reset_time,
            remaining_fraction,
        });

        if remaining_fraction < worst_remaining {
            worst_remaining = remaining_fraction;
            worst_model_id = model_id.clone();
        }
    }

    if details.is_empty() {
        return Err(CoreError::Internal(
            "no quota info found in response".into(),
        ));
    }

    let worst = details
        .iter()
        .find(|d| d.model_id == worst_model_id)
        .unwrap();

    Ok(AccountQuota {
        plan_name: Some("Antigravity".to_string()),
        session_used: Some(worst.session_used),
        session_limit: Some(worst.session_limit),
        session_reset_at: worst.session_reset_at.clone(),
        weekly_used: None,
        weekly_limit: None,
        weekly_reset_at: None,
        last_fetched_at: now_unix_secs_str(),
        fetch_error: None,
        model_details: Some(details),
    })
}

/// Fetch quota from the `retrieveUserQuotaSummary` endpoint. This
/// endpoint returns grouped quota buckets (weekly + 5h rolling windows)
/// with `remainingFraction` and `resetTime` per group.
async fn fetch_antigravity_user_quota(
    upstream: &Arc<UpstreamClient>,
    access_token: &str,
) -> Result<AccountQuota> {
    // Use retrieveUserQuotaSummary (not retrieveUserQuota). The Summary
    // variant returns a `groups` array with nested `buckets` — each
    // group represents a time window (weekly, 5h). The non-Summary
    // variant returns a flat `buckets` array without window labels.
    let endpoints = [
        "https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal:retrieveUserQuotaSummary",
        "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary",
        "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary",
    ];

    let mut last_err: Option<CoreError> = None;
    for url in &endpoints {
        let mut req = UpstreamRequest::post_json(*url, bytes::Bytes::from_static(b"{}"));
        if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {access_token}")) {
            req.headers.insert(http::header::AUTHORIZATION, v);
        }
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        crate::antigravity_headers::inject_antigravity_headers(&mut req.headers, None);

        let cancel = CancellationToken::new();
        let response = match upstream.call(req, TimeoutProfile::Quota, cancel).await {
            Ok(r) => r,
            Err(UpstreamError::Cancel) => return Err(CoreError::ClientDisconnected),
            Err(e) => {
                last_err = Some(CoreError::UpstreamConnection(format!(
                    "retrieveUserQuotaSummary: {e}"
                )));
                continue;
            }
        };

        if !response.status.is_success() {
            last_err = Some(CoreError::UpstreamConnection(format!(
                "retrieveUserQuotaSummary: status {}",
                response.status.as_u16()
            )));
            continue;
        }

        let body = match response.collect().await {
            Ok(b) => b,
            Err(e) => {
                last_err = Some(CoreError::UpstreamConnection(format!(
                    "retrieveUserQuotaSummary body: {e}"
                )));
                continue;
            }
        };

        let json: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(j) => j,
            Err(e) => {
                last_err = Some(CoreError::Parse(format!(
                    "retrieveUserQuotaSummary parse: {e}"
                )));
                continue;
            }
        };

        return parse_antigravity_user_quota_summary(&json);
    }

    Err(last_err.unwrap_or_else(|| {
        CoreError::UpstreamConnection("retrieveUserQuotaSummary: all endpoints failed".into())
    }))
}

/// Parse `retrieveUserQuotaSummary` response. The response has a `groups`
/// array, each group has a `displayName` and a `buckets` array. Each bucket
/// has `remainingFraction`, `resetTime`, `window` (e.g. "WEEKLY",
/// "FIVE_HOUR"), and `displayName`.
///
/// We extract:
/// - The WEEKLY bucket → `weekly_used` / `weekly_limit` / `weekly_reset_at`
/// - The FIVE_HOUR (or first non-weekly) bucket → `session_used` / `session_limit` / `session_reset_at`
fn parse_antigravity_user_quota_summary(body: &serde_json::Value) -> Result<AccountQuota> {
    const NORMALIZED_BASE: i64 = 1000;

    let groups = body
        .get("groups")
        .and_then(|g| g.as_array())
        .ok_or_else(|| {
            CoreError::Internal("missing 'groups' in retrieveUserQuotaSummary".into())
        })?;

    let mut weekly_used: Option<i64> = None;
    let mut weekly_limit: Option<i64> = None;
    let mut weekly_reset_at: Option<String> = None;
    let mut session_used: Option<i64> = None;
    let mut session_limit: Option<i64> = None;
    let mut session_reset_at: Option<String> = None;
    let mut plan_name: Option<String> = None;

    for group in groups {
        let group_plan = group.get("displayName").and_then(|n| n.as_str());

        let buckets = match group.get("buckets").and_then(|b| b.as_array()) {
            Some(b) => b,
            None => continue,
        };

        for bucket in buckets {
            let reset_time = bucket
                .get("resetTime")
                .and_then(|r| r.as_str())
                .map(String::from);
            let window = bucket.get("window").and_then(|w| w.as_str()).unwrap_or("");

            let remaining_fraction = bucket
                .get("remainingFraction")
                .and_then(|f| f.as_f64())
                .unwrap_or_else(|| if reset_time.is_some() { 0.0 } else { 1.0 });

            let is_unlimited = reset_time.is_none() && remaining_fraction >= 1.0;
            let remaining = (NORMALIZED_BASE as f64 * remaining_fraction) as i64;
            let used = if is_unlimited {
                0
            } else {
                NORMALIZED_BASE.saturating_sub(remaining)
            };

            // Route to weekly or session (5h) based on the window label.
            let is_weekly =
                window.to_uppercase().contains("WEEK") || window.eq_ignore_ascii_case("WEEKLY");
            if is_weekly && weekly_used.is_none() {
                weekly_used = Some(used);
                weekly_limit = Some(NORMALIZED_BASE);
                weekly_reset_at = reset_time;
                if plan_name.is_none() {
                    plan_name = group_plan.map(|s| s.to_string());
                }
            } else if !is_weekly && session_used.is_none() {
                // First non-weekly bucket (typically 5h) → session.
                session_used = Some(used);
                session_limit = Some(NORMALIZED_BASE);
                session_reset_at = reset_time;
                if plan_name.is_none() {
                    plan_name = group_plan.map(|s| s.to_string());
                }
            }
        }
    }

    if weekly_used.is_none() && session_used.is_none() {
        return Err(CoreError::Internal(
            "retrieveUserQuotaSummary: no usable buckets found".into(),
        ));
    }

    Ok(AccountQuota {
        session_used,
        session_limit,
        session_reset_at,
        weekly_used,
        weekly_limit,
        weekly_reset_at,
        plan_name: Some(plan_name.unwrap_or_else(|| "Antigravity".to_string())),
        last_fetched_at: now_unix_secs_str(),
        fetch_error: None,
        model_details: None,
    })
}

// =====================================================================
// Kiro AI (AWS CodeWhisperer)
// =====================================================================

/// Fetch quota for Kiro (AWS CodeWhisperer) using GetUsageLimits API.
pub async fn fetch_kiro_quota(
    upstream: &Arc<UpstreamClient>,
    access_token: &str,
    provider_specific: Option<&str>,
) -> Result<AccountQuota> {
    // 1. Resolve region and profile_arn from provider_specific metadata.
    let mut region = "us-east-1".to_string();
    let mut profile_arn = None;

    if let Some(json_str) = provider_specific
        && let Ok(meta) = serde_json::from_str::<serde_json::Value>(json_str)
    {
        if let Some(r) = meta.get("region").and_then(|v| v.as_str())
            && !r.is_empty()
        {
            region = r.to_string();
        }
        if let Some(arn) = meta.get("profileArn").and_then(|v| v.as_str()) {
            profile_arn = Some(arn.to_string());
        } else if let Some(arn) = meta.get("profile_arn").and_then(|v| v.as_str()) {
            profile_arn = Some(arn.to_string());
        }
    }

    let base_url = if region == "us-east-1" || region.is_empty() {
        "https://codewhisperer.us-east-1.amazonaws.com".to_string()
    } else {
        format!("https://q.{region}.amazonaws.com")
    };

    // 2. Discover profile_arn if missing
    let profile_arn = match profile_arn {
        Some(arn) => Some(arn),
        None => {
            // Call ListAvailableProfiles
            let url = format!("{base_url}/");
            let mut req =
                UpstreamRequest::post_json(&url, bytes::Bytes::from(r#"{"maxResults":10}"#));
            if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {access_token}")) {
                req.headers.insert(http::header::AUTHORIZATION, v);
            }
            req.headers.insert(
                http::header::HeaderName::from_static("x-amz-target"),
                http::HeaderValue::from_static("AmazonCodeWhispererService.ListAvailableProfiles"),
            );
            req.headers.insert(
                http::header::HeaderName::from_static("x-amz-user-agent"),
                http::HeaderValue::from_static("aws-sdk-js/3.0.0 kiro/0.1"),
            );

            let cancel = CancellationToken::new();

            match upstream.call(req, TimeoutProfile::OAuth, cancel).await {
                Ok(resp) if resp.status.is_success() => {
                    if let Ok(body_bytes) = resp.collect().await {
                        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body_bytes)
                        {
                            value
                                .get("profiles")
                                .and_then(|v| v.as_array())
                                .and_then(|arr| {
                                    arr.iter()
                                        .find(|p| {
                                            p.get("arn")
                                                .or_else(|| p.get("profileArn"))
                                                .and_then(|v| v.as_str())
                                                .map(|s| s.contains(&format!(":{region}:")))
                                                .unwrap_or(false)
                                        })
                                        .or_else(|| arr.first())
                                })
                                .and_then(|p| {
                                    p.get("arn")
                                        .or_else(|| p.get("profileArn"))
                                        .and_then(|v| v.as_str())
                                })
                                .map(|s| s.to_string())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                Ok(resp) => {
                    let status_code = resp.status;
                    let body_str =
                        String::from_utf8_lossy(&resp.collect().await.unwrap_or_default())
                            .to_string();
                    tracing::info!(status = %status_code, body = %body_str, "Kiro profile ARN discovery returned non-success; proceeding without profile ARN");
                    None
                }
                Err(e) => {
                    tracing::info!(error = %e, "kiro listAvailableProfiles network call failed; proceeding without profile ARN");
                    None
                }
            }
        }
    };

    // 3. Fetch GetUsageLimits
    let url = format!("{base_url}/");
    let mut payload = serde_json::json!({
        "origin": "AI_EDITOR",
        "resourceType": "AGENTIC_REQUEST"
    });
    if let Some(ref arn) = profile_arn {
        payload["profileArn"] = serde_json::json!(arn);
    }
    let body_bytes = match serde_json::to_vec(&payload) {
        Ok(b) => b,
        Err(e) => {
            tracing::info!(error = %e, "kiro GetUsageLimits serialize payload failed; returning empty quota");
            return Ok(AccountQuota {
                session_used: None,
                session_limit: None,
                session_reset_at: None,
                weekly_used: None,
                weekly_limit: None,
                weekly_reset_at: None,
                plan_name: Some("Kiro".to_string()),
                last_fetched_at: now_unix_secs_str(),
                fetch_error: None,
                model_details: None,
            });
        }
    };

    let mut req = UpstreamRequest::post_json(&url, bytes::Bytes::from(body_bytes));
    if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {access_token}")) {
        req.headers.insert(http::header::AUTHORIZATION, v);
    }
    req.headers.insert(
        http::header::HeaderName::from_static("x-amz-target"),
        http::HeaderValue::from_static("AmazonCodeWhispererService.GetUsageLimits"),
    );
    req.headers.insert(
        http::header::HeaderName::from_static("x-amz-user-agent"),
        http::HeaderValue::from_static("aws-sdk-js/3.0.0 kiro/0.1"),
    );

    let cancel = CancellationToken::new();
    let resp = match upstream.call(req, TimeoutProfile::OAuth, cancel).await {
        Ok(r) => r,
        Err(e) => {
            tracing::info!(error = %e, "kiro GetUsageLimits network call failed; returning empty quota without error");
            return Ok(AccountQuota {
                session_used: None,
                session_limit: None,
                session_reset_at: None,
                weekly_used: None,
                weekly_limit: None,
                weekly_reset_at: None,
                plan_name: Some("Kiro".to_string()),
                last_fetched_at: now_unix_secs_str(),
                fetch_error: None,
                model_details: None,
            });
        }
    };

    if !resp.status.is_success() {
        let status = resp.status.as_u16();
        let body_str =
            String::from_utf8_lossy(&resp.collect().await.unwrap_or_default()).to_string();
        tracing::info!(status = status, body = %body_str, "Kiro GetUsageLimits returned non-success (likely restricted quota access); returning empty quota without error");
        return Ok(AccountQuota {
            session_used: None,
            session_limit: None,
            session_reset_at: None,
            weekly_used: None,
            weekly_limit: None,
            weekly_reset_at: None,
            plan_name: Some("Kiro".to_string()),
            last_fetched_at: now_unix_secs_str(),
            fetch_error: None,
            model_details: None,
        });
    }

    let resp_bytes = resp
        .collect()
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("kiro GetUsageLimits read: {e}")))?;
    let data: serde_json::Value = serde_json::from_slice(&resp_bytes)
        .map_err(|e| CoreError::Parse(format!("kiro GetUsageLimits parse: {e}")))?;

    // 4. Parse response into AccountQuota
    let reset_at = data
        .get("nextDateReset")
        .or_else(|| data.get("resetDate"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let _overage_enabled = is_kiro_overage_enabled(&data);

    let usage_list = data.get("usageBreakdownList").and_then(|v| v.as_array());

    let mut session_used = None;
    let mut session_limit = None;

    if let Some(arr) = usage_list {
        for breakdown in arr {
            let resource_type = breakdown
                .get("resourceType")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if resource_type.to_lowercase() == "agentic_request" {
                let current = breakdown
                    .get("currentUsageWithPrecision")
                    .and_then(|v| v.as_f64())
                    .or_else(|| breakdown.get("currentUsage").and_then(|v| v.as_f64()))
                    .map(|v| v.round() as i64);
                let limit = breakdown
                    .get("usageLimitWithPrecision")
                    .and_then(|v| v.as_f64())
                    .or_else(|| breakdown.get("usageLimit").and_then(|v| v.as_f64()))
                    .map(|v| v.round() as i64);

                session_used = current;
                session_limit = limit;
                break;
            }
        }
    }

    let plan_name = data
        .get("subscriptionInfo")
        .and_then(|v| v.get("subscriptionTitle"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| Some("Kiro".to_string()));

    Ok(AccountQuota {
        session_used,
        session_limit,
        session_reset_at: reset_at,
        weekly_used: None,
        weekly_limit: None,
        weekly_reset_at: None,
        plan_name,
        last_fetched_at: now_unix_secs_str(),
        fetch_error: None,
        model_details: None,
    })
}

fn is_kiro_overage_enabled(data: &serde_json::Value) -> bool {
    let overage_status = data
        .get("overageConfiguration")
        .and_then(|v| v.get("overageStatus"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_uppercase();

    let overage_enabled_direct = data
        .get("overageEnabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let overage_enabled_config = data
        .get("overageConfiguration")
        .and_then(|v| v.get("overageEnabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    overage_status == "ENABLED" || overage_enabled_direct || overage_enabled_config
}

// =====================================================================
// Provider capabilities have moved to `ProviderAdapter::metadata()` and
// `ProviderMetadata`. The `quota_capable_providers` static list was removed.

// =====================================================================
// OpenRouter
// =====================================================================

/// Fetch the OpenRouter credit balance for an account.
///
/// Endpoint: `GET https://openrouter.ai/api/v1/key` with
/// `Authorization: Bearer <api_key>`. The response tracks CREDITS
/// (monetary) rather than request counts, so we map `usage` and
/// `limit` to the `session_used` / `session_limit` columns expressed
/// in cents (×100) for nicer display. `weekly_*` stays NULL — the
/// OpenRouter upstream has no separate weekly window.
///
/// On any error (network, non-2xx, parse) we return an
/// `AccountQuota` with all numeric fields `None` and a `fetch_error`
/// string describing the last failure, mirroring the contract of
/// [`fetch_minimax_quota`].
pub async fn fetch_openrouter_quota(
    upstream: &Arc<UpstreamClient>,
    api_key: &str,
) -> Result<AccountQuota> {
    let url = "https://openrouter.ai/api/v1/key";

    let mut req = UpstreamRequest::get(url);
    if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {api_key}")) {
        req.headers.insert(http::header::AUTHORIZATION, v);
    }

    let cancel = CancellationToken::new();
    let response = match upstream.call(req, TimeoutProfile::Quota, cancel).await {
        Ok(r) => r,
        Err(e) => {
            return Ok(AccountQuota {
                session_used: None,
                session_limit: None,
                session_reset_at: None,
                weekly_used: None,
                weekly_limit: None,
                weekly_reset_at: None,
                plan_name: None,
                last_fetched_at: now_unix_secs_str(),
                fetch_error: Some(format!("network: {e}")),
                model_details: None,
            });
        }
    };

    if !response.status.is_success() {
        // Capture the status before consuming the body — `collect`
        // moves the response into the buffer, so we can't borrow
        // the status after.
        let status = response.status.as_u16();
        // Truncate the body in the error message — the upstream
        // sometimes returns a long HTML error page and we don't want
        // it all sitting in the SQLite quota column.
        let body = response.collect().await.unwrap_or_default();
        let snippet = String::from_utf8_lossy(&body)
            .chars()
            .take(200)
            .collect::<String>();
        return Ok(AccountQuota {
            session_used: None,
            session_limit: None,
            session_reset_at: None,
            weekly_used: None,
            weekly_limit: None,
            weekly_reset_at: None,
            plan_name: None,
            last_fetched_at: now_unix_secs_str(),
            fetch_error: Some(format!("HTTP {}: {}", status, snippet)),
            model_details: None,
        });
    }

    let body = match response.collect().await {
        Ok(b) => b,
        Err(e) => {
            return Ok(AccountQuota {
                session_used: None,
                session_limit: None,
                session_reset_at: None,
                weekly_used: None,
                weekly_limit: None,
                weekly_reset_at: None,
                plan_name: None,
                last_fetched_at: now_unix_secs_str(),
                fetch_error: Some(format!("collect: {e}")),
                model_details: None,
            });
        }
    };

    let json: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => {
            return Ok(AccountQuota {
                session_used: None,
                session_limit: None,
                session_reset_at: None,
                weekly_used: None,
                weekly_limit: None,
                weekly_reset_at: None,
                plan_name: None,
                last_fetched_at: now_unix_secs_str(),
                fetch_error: Some(format!("parse: {e}")),
                model_details: None,
            });
        }
    };

    Ok(parse_openrouter_quota(&json, now_unix_secs_str()))
}

/// Parse the OpenRouter `/api/v1/key` response into our
/// `AccountQuota` shape.
///
/// Expected body:
/// ```json
/// {
///   "data": {
///     "usage": 0.015,            // credits used (dollars)
///     "limit": null,             // credit limit; null = unlimited
///     "is_free_tier": true,
///     "rate_limit": {
///       "requests": 20,
///       "interval": "m"          // "m" (minute) or "d" (day)
///     }
///   }
/// }
/// ```
///
/// **Sentinel handling.** OpenRouter uses `-1` (and the historical
/// equivalent of an explicit `0.0` for `limit`) to mean "not
/// configured / no limit applies" (the most common case for free-tier
/// keys that have no credit cap and a permissive rate limit, and for
/// any key the upstream hasn't finished provisioning). We treat every
/// negative numeric field — and `limit == 0.0` — as missing rather
/// than letting `-1` flow into the UI as a literal "$-1.00 used",
/// "$-0.01 limit" or "-1 req/10s" bar, or letting a `0.0` limit
/// produce a misleading "0 / 0" cell.
///
/// The `rate_limit.interval` field is supposed to be a single unit
/// letter (`"s"`, `"m"`, `"h"`, `"d"`) but the upstream has shipped
/// compound strings in the past (e.g. `"10s"` when the per-window
/// cap hasn't been normalised yet — the duration is glued to the
/// unit suffix). We take the **last** character defensively and
/// map it to a human-readable unit; anything we don't recognise
/// causes the rate-limit suffix to be dropped from the plan name
/// rather than printed as-is.
///
/// We convert dollars → cents (×100) for the `*_used` / `*_limit`
/// fields so the bar can render meaningful numbers. The `plan_name`
/// string picks up the rate-limit shape ("20 req/min") so the
/// operator can see it without opening devtools.
fn parse_openrouter_quota(body: &serde_json::Value, last_fetched_at: String) -> AccountQuota {
    let data = body.get("data");

    let raw_usage = data.and_then(|d| d.get("usage")).and_then(|v| v.as_f64());
    let raw_limit = data.and_then(|d| d.get("limit")).and_then(|v| v.as_f64());
    let is_free = data
        .and_then(|d| d.get("is_free_tier"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let rate_limit = data.and_then(|d| d.get("rate_limit"));

    // -1 → "no value configured". Cast negatives to None so they
    // never reach the bar math. A 0.0 limit is treated the same
    // way (the historical shape the upstream shipped before
    // switching to explicit `null` / `-1`); a 0.0 usage is kept as
    // Some(0) because that is a legitimate "no spend yet" reading
    // — only the limit's zero would render as the misleading
    // "0 / 0" cell.
    let session_used = raw_usage.filter(|u| *u >= 0.0).map(|u| (u * 100.0) as i64);
    let session_limit = raw_limit.filter(|l| *l > 0.0).map(|l| (l * 100.0) as i64);

    let plan_name = if is_free {
        "OpenRouter (free tier)".to_string()
    } else {
        "OpenRouter".to_string()
    };

    // Rate-limit shape: e.g. "20 req/min" or "1000 req/day". The
    // field may be absent (no rate limit applies), negative-sentinel
    // (not provisioned), or carry an unrecognised / compound unit
    // string. We bail to `None` in every case that would otherwise
    // print a misleading literal into the plan name.
    let rate_limit_text = rate_limit.and_then(format_rate_limit_suffix);

    let plan_name = match rate_limit_text {
        Some(rl) => format!("{} · {}", plan_name, rl),
        None => plan_name,
    };

    // Surface the "everything is missing" case as a soft error so
    // the UI shows "no usage data" instead of a misleading
    // "0 / 0" cell. A key whose credits are still being
    // provisioned returns exactly this shape (negative sentinels
    // or a zero limit), and treating it as "zero used out of zero
    // limit" hides a real configuration gap from the operator.
    let no_numeric_data = session_used.is_none() && session_limit.is_none();
    let fetch_error = if data.is_none() {
        Some("missing 'data' in response".to_string())
    } else if no_numeric_data {
        Some("usage not configured".to_string())
    } else {
        None
    };

    AccountQuota {
        session_used,
        session_limit,
        session_reset_at: None,
        weekly_used: None,
        weekly_limit: None,
        weekly_reset_at: None,
        plan_name: Some(plan_name),
        last_fetched_at,
        fetch_error,
        model_details: None,
    }
}

/// Format the rate-limit suffix that gets appended to the plan
/// name. Returns `None` whenever the upstream's data would print a
/// misleading value: missing fields, negative sentinels, or an
/// unrecognised unit character.
fn format_rate_limit_suffix(rl: &serde_json::Value) -> Option<String> {
    let reqs = rl.get("requests").and_then(|v| v.as_i64())?;
    let interval = rl.get("interval").and_then(|v| v.as_str())?;

    // Negative requests means "no rate limit provisioned" — the
    // upstream uses -1 for this. Don't print "-1 req/?" into the
    // plan name.
    if reqs < 0 {
        return None;
    }

    // OpenRouter's docs say interval is a single character, but the
    // upstream has shipped compound strings in the past ("10s",
    // "30m"). Take the first character and map known units; drop
    // the suffix entirely for anything else so the UI never shows
    // raw garbage.
    let unit = match interval.chars().last() {
        Some('s') => "sec",
        Some('m') => "min",
        Some('h') => "hr",
        Some('d') => "day",
        _ => return None,
    };

    Some(format!("{} req/{}", reqs, unit))
}

pub fn parse_reset_time(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Ok(secs) = s.parse::<u64>() {
        return Some(secs);
    }
    if let Ok(secs_f) = s.parse::<f64>() {
        return Some(secs_f.ceil() as u64);
    }
    let mut total_secs = 0.0;
    let mut num_str = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' {
            num_str.push(c);
        } else if matches!(c, 'h' | 'm' | 's') {
            let val = num_str.parse::<f64>().unwrap_or(0.0);
            match c {
                'h' => total_secs += val * 3600.0,
                'm' => total_secs += val * 60.0,
                's' => total_secs += val,
                _ => {}
            }
            num_str.clear();
        }
    }
    let total = total_secs.ceil() as u64;
    if total > 0 { Some(total) } else { None }
}

pub async fn fetch_codex_quota(
    upstream: &Arc<UpstreamClient>,
    access_token: &str,
    workspace_id: Option<&str>,
) -> Result<AccountQuota> {
    let url = "https://chatgpt.com/backend-api/wham/usage";
    let mut req = UpstreamRequest::get(url);
    req.headers.insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_str(&format!("Bearer {}", access_token))
            .unwrap_or_else(|_| http::HeaderValue::from_static("")),
    );
    req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );
    req.headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    req.headers.insert(
        http::header::HeaderName::from_static("origin"),
        http::HeaderValue::from_static("https://chatgpt.com"),
    );
    req.headers.insert(
        http::header::HeaderName::from_static("originator"),
        http::HeaderValue::from_static("codex_cli_rs"),
    );
    if let Ok(v) = http::HeaderValue::from_str(&crate::adapters::codex::codex_client_version()) {
        req.headers
            .insert(http::HeaderName::from_static("version"), v);
    }
    if let Ok(v) = http::HeaderValue::from_str(&crate::adapters::codex::codex_user_agent()) {
        req.headers.insert(http::header::USER_AGENT, v);
    }
    let workspace_header = workspace_id.and_then(codex_workspace_header);
    if let Some(ws) = workspace_header.as_deref()
        && let Ok(val) = http::HeaderValue::from_str(ws)
    {
        req.headers
            .insert(http::HeaderName::from_static("chatgpt-account-id"), val);
    }

    let cancel = CancellationToken::new();
    let response = upstream
        .call(req, TimeoutProfile::Chat, cancel)
        .await
        .map_err(|e| CoreError::UpstreamConnection(e.to_string()))?;

    let status = response.status.as_u16();
    if !(200..300).contains(&status) {
        let body = response.collect().await.unwrap_or_default();
        let snippet = String::from_utf8_lossy(&body)
            .chars()
            .take(200)
            .collect::<String>();
        return Ok(AccountQuota {
            session_used: None,
            session_limit: None,
            session_reset_at: None,
            weekly_used: None,
            weekly_limit: None,
            weekly_reset_at: None,
            plan_name: None,
            last_fetched_at: now_unix_secs_str(),
            fetch_error: Some(if snippet.is_empty() {
                format!("Codex quota check failed: HTTP {}", status)
            } else {
                format!("Codex quota check failed: HTTP {}: {}", status, snippet)
            }),
            model_details: None,
        });
    }

    let body = response
        .collect()
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("codex quota read: {e}")))?;
    let json: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| CoreError::Parse(format!("codex quota parse: {e}")))?;
    parse_codex_usage_quota(&json)
}

fn codex_workspace_header(provider_specific: &str) -> Option<String> {
    let raw = provider_specific.trim();
    if raw.is_empty() {
        return None;
    }
    if !raw.starts_with('{') {
        return Some(raw.to_string());
    }
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| {
            v.get("workspaceId")
                .or_else(|| v.get("workspace_id"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
        })
}

fn parse_codex_usage_quota(body: &serde_json::Value) -> Result<AccountQuota> {
    let rate_limit = body
        .get("rate_limit")
        .or_else(|| body.get("rateLimit"))
        .and_then(|v| v.as_object())
        .ok_or_else(|| CoreError::Parse("codex quota missing rate_limit".into()))?;

    let primary = rate_limit
        .get("primary_window")
        .or_else(|| rate_limit.get("primaryWindow"));
    let secondary = rate_limit
        .get("secondary_window")
        .or_else(|| rate_limit.get("secondaryWindow"));
    let (session_used, session_reset_at) = parse_codex_usage_window(primary);
    let (weekly_used, weekly_reset_at) = parse_codex_usage_window(secondary);

    Ok(AccountQuota {
        session_used,
        session_limit: session_used.map(|_| 100),
        session_reset_at,
        weekly_used,
        weekly_limit: weekly_used.map(|_| 100),
        weekly_reset_at,
        plan_name: Some("Codex / ChatGPT".into()),
        last_fetched_at: now_unix_secs_str(),
        fetch_error: None,
        model_details: None,
    })
}

fn parse_codex_usage_window(window: Option<&serde_json::Value>) -> (Option<i64>, Option<String>) {
    let Some(window) = window.and_then(|v| v.as_object()) else {
        return (None, None);
    };
    let used = window
        .get("used_percent")
        .or_else(|| window.get("usedPercent"))
        .and_then(json_f64)
        .map(|v| v.round().clamp(0.0, 100.0) as i64);
    let reset_at = window
        .get("reset_at")
        .or_else(|| window.get("resetAt"))
        .and_then(json_f64)
        .filter(|v| *v > 0.0)
        .map(|v| (v.ceil() as u64).to_string())
        .or_else(|| {
            window
                .get("reset_after_seconds")
                .or_else(|| window.get("resetAfterSeconds"))
                .and_then(json_f64)
                .filter(|v| *v > 0.0)
                .map(|v| {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    (now + v.ceil() as u64).to_string()
                })
        });
    (used, reset_at)
}

fn json_f64(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse::<f64>().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_quota_is_empty() {
        let q = AccountQuota {
            session_used: None,
            session_limit: None,
            session_reset_at: None,
            weekly_used: None,
            weekly_limit: None,
            weekly_reset_at: None,
            plan_name: None,
            last_fetched_at: "0".into(),
            fetch_error: None,
            model_details: None,
        };
        assert!(q.is_empty());

        let mut non_empty = q.clone();
        non_empty.fetch_error = Some("nope".into());
        assert!(!non_empty.is_empty());

        let mut non_empty2 = q.clone();
        non_empty2.session_used = Some(1);
        assert!(!non_empty2.is_empty());
    }

    #[test]
    fn parse_minimax_picks_general_entry() {
        let body = json!({
            "plan_name": "Coding Plan",
            "model_remains": [
                {
                    "model_name": "MiniMax-M2.5",
                    "current_interval_usage_count": 5,
                    "current_interval_total_count": 100,
                    "current_weekly_usage_count": 7,
                    "current_weekly_total_count": 70,
                },
                {
                    "model_name": "general",
                    "current_interval_usage_count": 42,
                    "current_interval_total_count": 5000,
                    "remains_time": 1_700_000_000_000_i64,
                    "current_weekly_usage_count": 8000,
                    "current_weekly_total_count": 50000,
                    "weekly_remains_time": 1_800_000_000_000_i64,
                },
            ]
        });
        let q = parse_minimax_quota(&body, "test://url").expect("parse");
        assert_eq!(q.plan_name.as_deref(), Some("Coding Plan"));
        assert_eq!(q.session_used, Some(42));
        assert_eq!(q.session_limit, Some(5000));
        assert_eq!(q.weekly_used, Some(8000));
        assert_eq!(q.weekly_limit, Some(50000));
        assert_eq!(q.session_reset_at.as_deref(), Some("1700000000"));
        assert_eq!(q.weekly_reset_at.as_deref(), Some("1800000000"));
        assert!(q.fetch_error.is_none());
    }

    #[test]
    fn parse_minimax_falls_back_to_first_entry() {
        // No `general` / `coding-plan` / `MiniMax-M*` entry present —
        // parser picks the first row to keep the quota from being
        // silently dropped.
        let body = json!({
            "plan_name": "x",
            "model_remains": [
                {
                    "model_name": "weird-tier",
                    "current_interval_usage_count": 1,
                    "current_interval_total_count": 2,
                },
            ]
        });
        let q = parse_minimax_quota(&body, "test://url").expect("parse");
        assert_eq!(q.session_used, Some(1));
        assert_eq!(q.session_limit, Some(2));
    }

    #[test]
    fn parse_minimax_errors_on_missing_model_remains() {
        let body = json!({ "plan_name": "x" });
        let err = parse_minimax_quota(&body, "test://url").expect_err("missing array");
        assert!(matches!(err, CoreError::Parse(_)));
    }

    #[test]
    fn parse_minimax_errors_on_empty_model_remains() {
        let body = json!({ "model_remains": [] });
        let err = parse_minimax_quota(&body, "test://url").expect_err("empty array");
        assert!(matches!(err, CoreError::Parse(_)));
    }

    #[test]
    fn parse_minimax_handles_percent_only_response() {
        // Real-world shape from the current coding_plan endpoint: the
        // `general` aggregate row ships only the remaining-percent
        // fields, no absolute counts.
        let body = json!({
            "plan_name": "MiniMax Coding Plan Lite",
            "model_remains": [
                {
                    "model_name": "MiniMax-M2.7",
                    "current_interval_total_count": 1500,
                    "current_interval_usage_count": 1100,
                    "current_weekly_total_count": 15000,
                    "current_weekly_usage_count": 13800
                },
                {
                    "model_name": "general",
                    "current_interval_remaining_percent": 25,
                    "current_weekly_remaining_percent": 50,
                    "remains_time": 300_000_i64,
                    "weekly_remains_time": 1_800_000_i64
                }
            ]
        });
        let q = parse_minimax_quota(&body, "test://url").expect("parse");
        // Aggregate row is preferred over the per-model one, so the
        // percent-derived values win.
        assert_eq!(q.session_used, Some(75)); // 100 - 25
        assert_eq!(q.session_limit, Some(100));
        assert_eq!(q.weekly_used, Some(50)); // 100 - 50
        assert_eq!(q.weekly_limit, Some(100));
        assert_eq!(q.plan_name.as_deref(), Some("MiniMax Coding Plan Lite"));
        assert!(q.fetch_error.is_none());
    }

    #[test]
    fn parse_minimax_prefers_counts_over_percent() {
        // If the upstream ever ships both shapes on the same row,
        // counts win — they're an exact measurement, percent is a
        // derived estimate.
        let body = json!({
            "model_remains": [
                {
                    "model_name": "general",
                    "current_interval_total_count": 1000,
                    "current_interval_usage_count": 250,
                    "current_weekly_total_count": 10000,
                    "current_weekly_usage_count": 5000,
                    "current_interval_remaining_percent": 75,
                    "current_weekly_remaining_percent": 50
                }
            ]
        });
        let q = parse_minimax_quota(&body, "test://url").expect("parse");
        assert_eq!(q.session_used, Some(250));
        assert_eq!(q.session_limit, Some(1000));
        assert_eq!(q.weekly_used, Some(5000));
        assert_eq!(q.weekly_limit, Some(10000));
    }

    #[test]
    fn parse_minimax_handles_zero_remaining_percent() {
        // Edge: window fully consumed → used=100.
        let body = json!({
            "model_remains": [
                {
                    "model_name": "general",
                    "current_interval_remaining_percent": 0,
                    "current_weekly_remaining_percent": 0
                }
            ]
        });
        let q = parse_minimax_quota(&body, "test://url").expect("parse");
        assert_eq!(q.session_used, Some(100));
        assert_eq!(q.session_limit, Some(100));
        assert_eq!(q.weekly_used, Some(100));
        assert_eq!(q.weekly_limit, Some(100));
    }

    #[test]
    fn parse_minimax_handles_100_remaining_percent() {
        // Edge: fresh window → used=0.
        let body = json!({
            "model_remains": [
                {
                    "model_name": "general",
                    "current_interval_remaining_percent": 100,
                    "current_weekly_remaining_percent": 100
                }
            ]
        });
        let q = parse_minimax_quota(&body, "test://url").expect("parse");
        assert_eq!(q.session_used, Some(0));
        assert_eq!(q.weekly_used, Some(0));
    }

    // ---- OpenRouter parser ----

    #[test]
    fn parse_openrouter_free_tier_with_rate_limit() {
        let body = json!({
            "data": {
                "usage": 0.015,
                "limit": null,
                "is_free_tier": true,
                "rate_limit": {
                    "requests": 20,
                    "interval": "m"
                }
            }
        });
        let q = parse_openrouter_quota(&body, "1700000000".into());
        // 0.015 dollars → 1 cent (truncation, not rounding)
        assert_eq!(q.session_used, Some(1));
        assert_eq!(q.session_limit, None);
        // Weekly fields stay None — OpenRouter has no weekly window.
        assert_eq!(q.weekly_used, None);
        assert_eq!(q.weekly_limit, None);
        // Plan name folds in the rate-limit shape, with "m" expanded
        // to "min" for readability.
        let plan = q.plan_name.as_deref().unwrap_or("");
        assert!(
            plan.contains("free"),
            "plan should mention free tier: {}",
            plan
        );
        assert!(
            plan.contains("20 req/min"),
            "plan should include rate limit: {}",
            plan
        );
        assert!(q.fetch_error.is_none());
        assert_eq!(q.last_fetched_at, "1700000000");
    }

    #[test]
    fn parse_openrouter_paid_tier_with_limit() {
        let body = json!({
            "data": {
                "usage": 12.5,
                "limit": 100.0,
                "is_free_tier": false,
                "rate_limit": {
                    "requests": 1000,
                    "interval": "d"
                }
            }
        });
        let q = parse_openrouter_quota(&body, "0".into());
        // 12.5 * 100 = 1250 cents; 100 * 100 = 10000 cents.
        assert_eq!(q.session_used, Some(1250));
        assert_eq!(q.session_limit, Some(10000));
        let plan = q.plan_name.as_deref().unwrap_or("");
        assert!(plan.contains("OpenRouter"));
        assert!(
            !plan.contains("free"),
            "paid tier should not say free: {}",
            plan
        );
        assert!(
            plan.contains("1000 req/day"),
            "plan should include rate limit: {}",
            plan
        );
    }

    #[test]
    fn parse_openrouter_missing_data_object_marks_error() {
        // No `data` key — the body is unrecognised. The parser
        // surfaces this as a soft error so the UI shows
        // "missing 'data' in response" instead of an empty bar
        // that could be mistaken for a real zero quota.
        let body = json!({ "unrelated": true });
        let q = parse_openrouter_quota(&body, "0".into());
        assert!(q.session_used.is_none());
        assert!(q.session_limit.is_none());
        assert!(q.plan_name.as_deref().unwrap_or("").contains("OpenRouter"));
        assert!(q.fetch_error.is_some());
    }

    #[test]
    fn parse_openrouter_maps_known_interval_units() {
        // All four documented unit letters expand to a human label
        // in the plan name. No raw letter is left in the string.
        for (raw, label) in [("s", "sec"), ("m", "min"), ("h", "hr"), ("d", "day")] {
            let body = json!({
                "data": {
                    "usage": 0.0,
                    "limit": null,
                    "is_free_tier": true,
                    "rate_limit": { "requests": 7, "interval": raw }
                }
            });
            let q = parse_openrouter_quota(&body, "0".into());
            let plan = q.plan_name.as_deref().unwrap_or("");
            assert!(
                plan.contains(&format!("7 req/{}", label)),
                "interval {:?} should map to {:?}, got plan: {}",
                raw,
                label,
                plan
            );
        }
    }

    #[test]
    fn parse_openrouter_drops_unknown_interval_unit() {
        // Unrecognised unit character (here "x") means we can't
        // present a meaningful rate-limit label, so the suffix is
        // dropped from the plan name. The plan itself still
        // surfaces so the operator can see the account is on
        // OpenRouter.
        let body = json!({
            "data": {
                "usage": 1.0,
                "limit": 10.0,
                "is_free_tier": false,
                "rate_limit": {
                    "requests": 5,
                    "interval": "x"
                }
            }
        });
        let q = parse_openrouter_quota(&body, "0".into());
        let plan = q.plan_name.as_deref().unwrap_or("");
        assert!(plan.contains("OpenRouter"));
        assert!(
            !plan.contains("5 req"),
            "plan should drop rate-limit suffix on unknown unit: {}",
            plan
        );
    }

    #[test]
    fn parse_openrouter_takes_first_char_of_compound_interval() {
        // Defensive: the upstream has shipped compound strings like
        // "10s" in the past. Take the first character and map it
        // like any other unit.
        let body = json!({
            "data": {
                "usage": 0.0,
                "limit": null,
                "is_free_tier": true,
                "rate_limit": {
                    "requests": 3,
                    "interval": "10s"
                }
            }
        });
        let q = parse_openrouter_quota(&body, "0".into());
        let plan = q.plan_name.as_deref().unwrap_or("");
        assert!(plan.contains("3 req/sec"), "got plan: {}", plan);
        assert!(
            !plan.contains("10s"),
            "compound interval must not appear verbatim: {}",
            plan
        );
    }

    #[test]
    fn parse_openrouter_treats_negative_usage_and_limit_as_missing() {
        // OpenRouter uses -1 to mean "not configured". -1 must
        // never reach the UI as a literal "$-1.00 used" or "$-1
        // limit"; convert to None.
        let body = json!({
            "data": {
                "usage": -1.0,
                "limit": -1.0,
                "is_free_tier": true,
                "rate_limit": {
                    "requests": -1,
                    "interval": "10s"
                }
            }
        });
        let q = parse_openrouter_quota(&body, "0".into());
        assert_eq!(q.session_used, None);
        assert_eq!(q.session_limit, None);
        let plan = q.plan_name.as_deref().unwrap_or("");
        assert!(plan.contains("OpenRouter"));
        assert!(
            !plan.contains("-1"),
            "plan must not contain literal -1: {}",
            plan
        );
        assert!(
            !plan.contains("10s"),
            "plan must not contain compound interval: {}",
            plan
        );
        // All-sentinel shape is a soft error so the UI doesn't
        // pretend the account has a zero quota.
        assert!(q.fetch_error.is_some());
    }

    #[test]
    fn parse_openrouter_treats_null_limit_as_unlimited() {
        // Explicit null limit (the documented "unlimited" value)
        // stays None. Negative limit is the same outcome via a
        // different path; both must collapse to None.
        for limit in [None, Some(serde_json::Value::from(-1.0))] {
            let body = json!({
                "data": {
                    "usage": 0.0,
                    "limit": limit,
                    "is_free_tier": false,
                    "rate_limit": { "requests": 100, "interval": "m" }
                }
            });
            let q = parse_openrouter_quota(&body, "0".into());
            assert_eq!(q.session_limit, None, "limit {:?} should be None", limit);
            assert_eq!(q.session_used, Some(0));
        }
    }

    #[test]
    fn parse_openrouter_treats_zero_limit_as_unlimited() {
        // A historical shape the upstream shipped: `limit: 0.0` as
        // the "unlimited / not configured" marker. If we passed
        // that through as Some(0) the UI would render "0 / 0" for
        // every fresh account, which is exactly the bug the user
        // reported. Usage stays Some(0) — "no spend yet" is a
        // legitimate reading; only the zero limit is the sentinel.
        let body = json!({
            "data": {
                "usage": 0.0,
                "limit": 0.0,
                "is_free_tier": false,
                "rate_limit": { "requests": 100, "interval": "m" }
            }
        });
        let q = parse_openrouter_quota(&body, "0".into());
        assert_eq!(q.session_limit, None);
        assert_eq!(q.session_used, Some(0));
        // The user has spend data, just no configured cap, so this
        // is a healthy "0 / unlimited" reading, not a configuration
        // gap.
        assert!(q.fetch_error.is_none());
    }

    #[test]
    fn parse_openrouter_small_usage_truncates_to_zero() {
        // Sub-cent usage truncates to 0 cents. This is the
        // expected behaviour: anything < $0.01 is noise, and
        // "$0.00 used out of N" is a fine display.
        let body = json!({
            "data": {
                "usage": 0.004,
                "limit": 10.0,
                "is_free_tier": false,
                "rate_limit": { "requests": 100, "interval": "m" }
            }
        });
        let q = parse_openrouter_quota(&body, "0".into());
        assert_eq!(q.session_used, Some(0));
        assert_eq!(q.session_limit, Some(1000));
    }

    // ---- Antigravity parser tests ----

    #[test]
    fn parse_antigravity_models_response_with_quota_info() {
        let body = json!({
            "models": {
                "claude-sonnet-4": {
                    "quotaInfo": {
                        "remainingFraction": 0.6,
                        "resetTime": "2025-01-01T00:00:00Z"
                    }
                }
            }
        });
        let q = parse_antigravity_models_response(&body).expect("parse");
        assert_eq!(q.session_used, Some(400)); // 1000 * (1 - 0.6)
        assert_eq!(q.session_limit, Some(1000));
        assert_eq!(q.session_reset_at.as_deref(), Some("2025-01-01T00:00:00Z"));
        assert!(q.plan_name.unwrap().contains("claude-sonnet-4"));
        assert!(q.fetch_error.is_none());
    }

    #[test]
    fn parse_antigravity_models_response_unlimited() {
        // No resetTime and remainingFraction >= 1.0 → unlimited.
        let body = json!({
            "models": {
                "gemini-pro": {
                    "quotaInfo": {
                        "remainingFraction": 1.0
                    }
                }
            }
        });
        let q = parse_antigravity_models_response(&body).expect("parse");
        assert_eq!(q.session_used, Some(0));
        assert_eq!(q.session_limit, Some(1000));
        assert!(q.session_reset_at.is_none());
    }

    #[test]
    fn parse_antigravity_models_response_missing_models() {
        let body = json!({ "not_models": {} });
        let err = parse_antigravity_models_response(&body).expect_err("missing");
        assert!(matches!(err, CoreError::Internal(_)));
    }

    #[test]
    fn parse_antigravity_models_response_no_quota_info() {
        let body = json!({
            "models": {
                "model-a": {}
            }
        });
        let err = parse_antigravity_models_response(&body).expect_err("no quota info");
        assert!(matches!(err, CoreError::Internal(_)));
    }

    #[test]
    fn parse_antigravity_user_quota_summary_with_buckets() {
        // retrieveUserQuotaSummary response: a `groups` array, each group
        // contains a `buckets` array. Each bucket has `remainingFraction`,
        // `resetTime`, and a `window` label (WEEKLY / FIVE_HOUR / etc).
        let body = json!({
            "groups": [
                {
                    "displayName": "Antigravity",
                    "buckets": [
                        {
                            "remainingFraction": 0.35,
                            "resetTime": "2025-06-01T12:00:00Z",
                            "window": "FIVE_HOUR",
                            "displayName": "5h rolling"
                        },
                        {
                            "remainingFraction": 0.6,
                            "resetTime": "2025-06-07T00:00:00Z",
                            "window": "WEEKLY",
                            "displayName": "Weekly rolling"
                        }
                    ]
                }
            ]
        });
        let q = parse_antigravity_user_quota_summary(&body).expect("parse");
        // FIVE_HOUR bucket → session
        assert_eq!(q.session_used, Some(650)); // 1000 * (1 - 0.35)
        assert_eq!(q.session_limit, Some(1000));
        assert_eq!(q.session_reset_at.as_deref(), Some("2025-06-01T12:00:00Z"));
        // WEEKLY bucket → weekly
        assert_eq!(q.weekly_used, Some(400)); // 1000 * (1 - 0.6)
        assert_eq!(q.weekly_limit, Some(1000));
        assert_eq!(q.weekly_reset_at.as_deref(), Some("2025-06-07T00:00:00Z"));
        assert_eq!(q.plan_name.as_deref(), Some("Antigravity"));
        assert!(q.fetch_error.is_none());
    }

    #[test]
    fn parse_antigravity_user_quota_summary_missing_groups() {
        let body = json!({ "not_groups": [] });
        let err = parse_antigravity_user_quota_summary(&body).expect_err("missing groups");
        assert!(matches!(err, CoreError::Internal(_)));
    }

    #[test]
    fn parse_antigravity_user_quota_summary_empty_groups() {
        // groups present but no buckets anywhere → no usable buckets.
        let body = json!({ "groups": [] });
        let err = parse_antigravity_user_quota_summary(&body).expect_err("empty groups");
        assert!(matches!(err, CoreError::Internal(_)));
    }

    #[test]
    fn codex_workspace_header_accepts_raw_and_json() {
        assert_eq!(
            codex_workspace_header("acc_raw").as_deref(),
            Some("acc_raw")
        );
        assert_eq!(
            codex_workspace_header(r#"{"workspaceId":"acc_json"}"#).as_deref(),
            Some("acc_json")
        );
        assert_eq!(
            codex_workspace_header(r#"{"workspace_id":"acc_snake"}"#).as_deref(),
            Some("acc_snake")
        );
        assert_eq!(codex_workspace_header("{}"), None);
    }

    #[test]
    fn parse_codex_usage_quota_maps_windows() {
        let body = json!({
            "rate_limit": {
                "primary_window": {
                    "used_percent": 17.4,
                    "reset_at": 1783434000
                },
                "secondary_window": {
                    "used_percent": "48.6",
                    "reset_at": "1784038800"
                }
            }
        });
        let q = parse_codex_usage_quota(&body).expect("parse");
        assert_eq!(q.session_used, Some(17));
        assert_eq!(q.session_limit, Some(100));
        assert_eq!(q.session_reset_at.as_deref(), Some("1783434000"));
        assert_eq!(q.weekly_used, Some(49));
        assert_eq!(q.weekly_limit, Some(100));
        assert_eq!(q.weekly_reset_at.as_deref(), Some("1784038800"));
        assert_eq!(q.plan_name.as_deref(), Some("Codex / ChatGPT"));
    }

    #[test]
    fn test_is_kiro_overage_enabled() {
        let body = json!({
            "overageConfiguration": {
                "overageStatus": "ENABLED"
            }
        });
        assert!(is_kiro_overage_enabled(&body));

        let body2 = json!({
            "overageEnabled": true
        });
        assert!(is_kiro_overage_enabled(&body2));

        let body3 = json!({
            "overageConfiguration": {
                "overageEnabled": true
            }
        });
        assert!(is_kiro_overage_enabled(&body3));

        let body_disabled = json!({
            "overageConfiguration": {
                "overageStatus": "DISABLED"
            }
        });
        assert!(!is_kiro_overage_enabled(&body_disabled));
    }
}
