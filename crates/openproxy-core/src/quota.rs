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
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Quota snapshot for a single account.
///
/// All numeric fields are `Option<i64>` because the upstream may omit
/// them (e.g. an account with no rate limit). `last_fetched_at` is the
/// only always-present field: it records when the snapshot was taken,
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
    pub session_reset_at: Option<String>,  // ISO-8601 or epoch seconds
    pub weekly_used: Option<i64>,
    pub weekly_limit: Option<i64>,
    pub weekly_reset_at: Option<String>,
    pub plan_name: Option<String>,
    pub last_fetched_at: String,
    pub fetch_error: Option<String>,
}

impl AccountQuota {
    /// True when no useful data is present: no usage counters, no
    /// reset times, and no error message. The UI uses this to decide
    /// between "not fetched yet" (no rows ever written) and "fetched,
    /// but the upstream said nothing".
    pub fn is_empty(&self) -> bool {
        self.session_used.is_none()
            && self.weekly_used.is_none()
            && self.fetch_error.is_none()
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
    http: &reqwest::Client,
    api_key: &str,
) -> Result<AccountQuota> {
    let urls = [
        "https://api.minimax.io/v1/token_plan/remains",
        "https://api.minimax.io/v1/api/openplatform/coding_plan/remains",
    ];

    let mut last_err: Option<String> = None;
    for url in &urls {
        match fetch_minimax_from_url(http, api_key, url).await {
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
    })
}

/// GET a single MiniMax quota endpoint, returning either a parsed
/// `AccountQuota` or a `CoreError` describing the failure. Used
/// internally by [`fetch_minimax_quota`]'s fallback loop.
async fn fetch_minimax_from_url(
    http: &reqwest::Client,
    api_key: &str,
    url: &str,
) -> Result<AccountQuota> {
    let resp = http
        .get(url)
        .header("Authorization", format!("Bearer {}", api_key))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("{}: {}", url, e)))?;

    if !resp.status().is_success() {
        return Err(CoreError::UpstreamConnection(format!(
            "{}: status {}",
            url,
            resp.status().as_u16()
        )));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| CoreError::Parse(format!("{}: {}", url, e)))?;

    parse_minimax_quota(&body, url)
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
        .ok_or_else(|| {
            CoreError::Parse(format!(
                "{}: missing 'model_remains' array",
                url
            ))
        })?;

    if entries.is_empty() {
        return Err(CoreError::Parse(format!("{}: empty model_remains", url)));
    }

    let target = entries
        .iter()
        .find(|e| {
            let name = e
                .get("model_name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
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
                let name = e
                    .get("model_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
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
    if let (Some(u), Some(l)) = (used, limit) {
        if l > 0 {
            return (Some(u), Some(l));
        }
    }

    // 2. Percent: the upstream exposes "remaining" (e.g. 25 = 25%
    //    of the window left), not "used". Invert it and pin the
    //    limit at 100 so the bar math is unchanged. Negative inputs
    //    or values above 100 are rejected — the upstream shouldn't
    //    send them, and trusting them would let a bug paint a 200%
    //    bar.
    let remaining = entry.get(remaining_pct_key).and_then(|v| v.as_i64());
    if let Some(rp) = remaining {
        if (0..=100).contains(&rp) {
            let used_calc = (100 - rp).max(0);
            return (Some(used_calc), Some(100));
        }
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
/// token. Tries `fetchAvailableModels` first; on failure, falls back to
/// `retrieveUserQuota`.
///
/// `access_token` is the *plaintext* OAuth access token — the caller
/// is responsible for decrypting it from the account row.
pub async fn fetch_antigravity_quota(
    http: &reqwest::Client,
    access_token: &str,
) -> Result<AccountQuota> {
    match fetch_antigravity_models_quota(http, access_token).await {
        Ok(quota) => Ok(quota),
        Err(_) => fetch_antigravity_user_quota(http, access_token).await,
    }
}

/// Fetch quota from the `fetchAvailableModels` endpoint.
async fn fetch_antigravity_models_quota(
    http: &reqwest::Client,
    access_token: &str,
) -> Result<AccountQuota> {
    let endpoints = [
        "https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels",
        "https://cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels",
    ];

    let ua = "Antigravity/4.2.0 (X11; Linux x86_64) Chrome/132.0.6834.160 Electron/39.2.3";

    for endpoint in &endpoints {
        let response = http
            .post(*endpoint)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Content-Type", "application/json")
            .header("User-Agent", ua)
            .header("X-Goog-Api-Client", "gl-node/22.21.1")
            .body("{}".to_string())
            .timeout(Duration::from_secs(15))
            .send()
            .await;

        if let Ok(resp) = response {
            if resp.status().is_success() {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    return parse_antigravity_models_response(&body);
                }
            }
        }
    }

    Err(CoreError::UpstreamConnection(
        "all fetchAvailableModels endpoints failed".into(),
    ))
}

/// Parse `fetchAvailableModels` response into `AccountQuota`.
fn parse_antigravity_models_response(body: &serde_json::Value) -> Result<AccountQuota> {
    const NORMALIZED_BASE: i64 = 1000;

    let models = body
        .get("models")
        .and_then(|m| m.as_object())
        .ok_or_else(|| CoreError::Internal("missing 'models' in response".into()))?;

    for (model_id, model_data) in models {
        if let Some(quota_info) = model_data.get("quotaInfo") {
            let remaining_fraction = quota_info
                .get("remainingFraction")
                .and_then(|f| f.as_f64())
                .unwrap_or(1.0);

            let reset_time = quota_info
                .get("resetTime")
                .and_then(|r| r.as_str())
                .map(String::from);

            let is_unlimited = reset_time.is_none() && remaining_fraction >= 1.0;

            let remaining = (NORMALIZED_BASE as f64 * remaining_fraction) as i64;
            let used = if is_unlimited {
                0
            } else {
                NORMALIZED_BASE.saturating_sub(remaining)
            };

            return Ok(AccountQuota {
                plan_name: Some(format!("Antigravity ({})", model_id)),
                session_used: Some(used),
                session_limit: Some(NORMALIZED_BASE),
                session_reset_at: reset_time,
                weekly_used: None,
                weekly_limit: None,
                weekly_reset_at: None,
                last_fetched_at: now_unix_secs_str(),
                fetch_error: None,
            });
        }
    }

    Err(CoreError::Internal(
        "no quota info found in response".into(),
    ))
}

/// Fetch quota from the `retrieveUserQuota` endpoint.
async fn fetch_antigravity_user_quota(
    http: &reqwest::Client,
    access_token: &str,
) -> Result<AccountQuota> {
    let resp = http
        .post("https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/json")
        .body("{}".to_string())
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("retrieveUserQuota: {e}")))?;

    if !resp.status().is_success() {
        return Err(CoreError::UpstreamConnection(format!(
            "retrieveUserQuota: status {}",
            resp.status().as_u16()
        )));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| CoreError::Parse(format!("retrieveUserQuota parse: {e}")))?;

    parse_antigravity_user_quota_response(&body)
}

/// Parse `retrieveUserQuota` response into `AccountQuota`.
fn parse_antigravity_user_quota_response(body: &serde_json::Value) -> Result<AccountQuota> {
    const NORMALIZED_BASE: i64 = 1000;

    let buckets = body
        .get("buckets")
        .and_then(|b| b.as_array())
        .ok_or_else(|| CoreError::Internal("missing 'buckets' in response".into()))?;

    for bucket in buckets {
        let remaining_fraction = bucket
            .get("remainingFraction")
            .and_then(|f| f.as_f64())
            .unwrap_or(1.0);

        let reset_time = bucket
            .get("resetTime")
            .and_then(|r| r.as_str())
            .map(String::from);

        let is_unlimited = reset_time.is_none() && remaining_fraction >= 1.0;

        let remaining = (NORMALIZED_BASE as f64 * remaining_fraction) as i64;
        let used = if is_unlimited {
            0
        } else {
            NORMALIZED_BASE.saturating_sub(remaining)
        };

        return Ok(AccountQuota {
            plan_name: Some("Antigravity".to_string()),
            session_used: Some(used),
            session_limit: Some(NORMALIZED_BASE),
            session_reset_at: reset_time,
            weekly_used: None,
            weekly_limit: None,
            weekly_reset_at: None,
            last_fetched_at: now_unix_secs_str(),
            fetch_error: None,
        });
    }

    Err(CoreError::Internal("no buckets in response".into()))
}

// =====================================================================
// Provider capability registry
// =====================================================================

/// The list of provider ids that have a quota endpoint we know how to
/// call today. The HTTP handler uses this list to short-circuit a quota
/// refresh with a `{"supported": false}` body when the caller asks
/// about an unsupported provider.
///
/// This is a static list (not a DB table) because the set is extremely
/// stable — it only changes when a new fetcher lands, and that always
/// coincides with a code change. See `fetch_account_quota` in the
/// `admin` module for the dispatch that pairs with this list.
pub fn quota_capable_providers() -> &'static [&'static str] {
    &[
        "minimax",
        "minimax-cn",
        "openrouter",
        "antigravity",
        "antigravity-cli",
        "agy",
    ]
}

/// Convenience wrapper used by the HTTP handler and the front-end
/// mirror: is `provider_id` in the supported set?
pub fn supports_quota(provider_id: &str) -> bool {
    quota_capable_providers()
        .iter()
        .any(|p| *p == provider_id)
}

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
    http: &reqwest::Client,
    api_key: &str,
) -> Result<AccountQuota> {
    let url = "https://openrouter.ai/api/v1/key";

    let resp = match http
        .get(url)
        .header("Authorization", format!("Bearer {}", api_key))
        .timeout(Duration::from_secs(15))
        .send()
        .await
    {
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
                fetch_error: Some(format!("network: {}", e)),
            });
        }
    };

    if !resp.status().is_success() {
        // Capture the status before consuming the body — `Response::text`
        // takes `self` by value, so we can't borrow the status after.
        let status = resp.status().as_u16();
        // Truncate the body in the error message — the upstream
        // sometimes returns a long HTML error page and we don't want
        // it all sitting in the SQLite quota column.
        let snippet = resp
            .text()
            .await
            .unwrap_or_default()
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
        });
    }

    let body: serde_json::Value = match resp.json().await {
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
                fetch_error: Some(format!("parse: {}", e)),
            });
        }
    };

    Ok(parse_openrouter_quota(&body, now_unix_secs_str()))
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
    let session_used = raw_usage
        .filter(|u| *u >= 0.0)
        .map(|u| (u * 100.0) as i64);
    let session_limit = raw_limit
        .filter(|l| *l > 0.0)
        .map(|l| (l * 100.0) as i64);

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

    // ---- capability registry ----

    #[test]
    fn quota_capable_providers_includes_openrouter() {
        let list = quota_capable_providers();
        assert!(list.contains(&"minimax"));
        assert!(list.contains(&"minimax-cn"));
        assert!(list.contains(&"openrouter"));
    }

    #[test]
    fn supports_quota_matches_registry() {
        assert!(supports_quota("minimax"));
        assert!(supports_quota("minimax-cn"));
        assert!(supports_quota("openrouter"));
        assert!(!supports_quota("opencode-zen"));
        assert!(!supports_quota("custom-provider"));
        assert!(!supports_quota(""));
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
        assert!(plan.contains("free"), "plan should mention free tier: {}", plan);
        assert!(plan.contains("20 req/min"), "plan should include rate limit: {}", plan);
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
        assert!(!plan.contains("free"), "paid tier should not say free: {}", plan);
        assert!(plan.contains("1000 req/day"), "plan should include rate limit: {}", plan);
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
        assert!(!plan.contains("10s"), "compound interval must not appear verbatim: {}", plan);
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
        assert!(!plan.contains("-1"), "plan must not contain literal -1: {}", plan);
        assert!(!plan.contains("10s"), "plan must not contain compound interval: {}", plan);
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
        assert_eq!(
            q.session_reset_at.as_deref(),
            Some("2025-01-01T00:00:00Z")
        );
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
    fn parse_antigravity_user_quota_response_with_buckets() {
        let body = json!({
            "buckets": [
                {
                    "remainingFraction": 0.35,
                    "resetTime": "2025-06-01T12:00:00Z"
                }
            ]
        });
        let q = parse_antigravity_user_quota_response(&body).expect("parse");
        assert_eq!(q.session_used, Some(650)); // 1000 * (1 - 0.35)
        assert_eq!(q.session_limit, Some(1000));
        assert_eq!(
            q.session_reset_at.as_deref(),
            Some("2025-06-01T12:00:00Z")
        );
        assert_eq!(q.plan_name.as_deref(), Some("Antigravity"));
        assert!(q.fetch_error.is_none());
    }

    #[test]
    fn parse_antigravity_user_quota_response_missing_buckets() {
        let body = json!({ "not_buckets": [] });
        let err =
            parse_antigravity_user_quota_response(&body).expect_err("missing buckets");
        assert!(matches!(err, CoreError::Internal(_)));
    }

    #[test]
    fn parse_antigravity_user_quota_response_empty_buckets() {
        let body = json!({ "buckets": [] });
        let err =
            parse_antigravity_user_quota_response(&body).expect_err("empty buckets");
        assert!(matches!(err, CoreError::Internal(_)));
    }

    #[test]
    fn quota_capable_providers_includes_antigravity() {
        let list = quota_capable_providers();
        assert!(list.contains(&"antigravity"));
        assert!(list.contains(&"antigravity-cli"));
        assert!(list.contains(&"agy"));
    }

    #[test]
    fn supports_quota_matches_antigravity() {
        assert!(supports_quota("antigravity"));
        assert!(supports_quota("antigravity-cli"));
        assert!(supports_quota("agy"));
    }
}
