use super::dual::status_pool;
use super::envelope::envelope_is_success;
use crate::adapters::{
    CancellationToken, CoreError, Result, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use openproxy_types::quota::{QuotaPool, QuotaPoolStatus, QuotaSource};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

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

// ---------------------------------------------------------------------------
// Daily check-in
// ---------------------------------------------------------------------------

const APP_ID: &str = "100003";
const TASK_APP_KEY: &str = "6c79a4059ae078650bc7886b627cf9f9";
const USER_INFO_APP_KEY: &str = "38d2391985e2369a5fb8227d8e6cd5e5";

const TASK_COMPLETE_URL: &str =
    "https://autoglm-api.autoglm.ai/autoclaw-proxy/proxy/autoclaw-task-complete";
const TASK_LIST_URL: &str =
    "https://autoglm-api.autoglm.ai/autoclaw-proxy/proxy/autoclaw-task-list";
const USER_INFO_URL: &str = "https://autoglm-api.autoglm.ai/userapi/v1/user-info";

const DAILY_SIGNIN_TASK: &str = "daily_signin";

/// Client-triggerable AutoClaw tasks. Idempotent upstream: re-issuing one that
/// is already claimed answers `already_completed` and costs nothing.
const CLAIMABLE_TASKS: [&str; 5] = [
    "daily_signin",
    "daily_inspiration_center",
    "newbie_cloud_lobster",
    "newbie_local_lobster",
    "download_pc_app",
];

/// Spacing the upstream expects between burst claims.
const CLAIM_SPACING: Duration = Duration::from_millis(1500);

/// Dedupe window for concurrent invocations carrying the same credential.
///
/// The authoritative "already done today" gate is the upstream task list (see
/// [`fetch_task_list`]), which survives restarts and knows the provider's real
/// day boundary. This window only collapses duplicate concurrent runs, e.g. an
/// hourly quota sync overlapping a manual dashboard refresh. It is deliberately
/// short: a long window silently suppresses the next day's claim, which is
/// exactly how the previous 6 h in-memory throttle broke the daily bonus.
const CHECKIN_DEDUPE_WINDOW_SECS: u64 = 60;

/// Signing profile per endpoint family. The task endpoints and the desktop
/// `user-info` endpoint use different app keys, timestamp units, locales and
/// client identities; mixing them makes the upstream reject the call.
struct SignProfile {
    app_key: &'static str,
    timestamp_millis: bool,
    lang: &'static str,
    client_type: &'static str,
    tm: &'static str,
    version: &'static str,
}

const TASK_PROFILE: SignProfile = SignProfile {
    app_key: TASK_APP_KEY,
    timestamp_millis: false,
    lang: "es",
    client_type: "web",
    tm: "web",
    version: "1.12.1",
};

const USER_INFO_PROFILE: SignProfile = SignProfile {
    app_key: USER_INFO_APP_KEY,
    timestamp_millis: true,
    lang: "zh-CN",
    client_type: "pc",
    tm: "linux",
    version: "2.0.3",
};

const CHECKIN_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36";

/// Result of one check-in pass for a single AutoClaw account.
pub struct AutoclawCheckinResult {
    /// UTC date the check-in is recorded for. The dashboard compares it
    /// verbatim, so it must use the same clock and format.
    pub last_checkin_date: String,
    /// Consecutive-day streak reported by the upstream task list.
    pub streak_days: u32,
    /// `true` when the upstream already had today's `daily_signin` completed
    /// and no claim request was issued.
    pub already_claimed: bool,
}

/// UTC calendar date, matching the format the dashboard compares against.
pub fn today_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

fn checkin_dedupe_allows(token: &str, now_secs: u64) -> bool {
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
        Some(last) if now_secs.saturating_sub(*last) < CHECKIN_DEDUPE_WINDOW_SECS => false,
        _ => {
            guard.insert(key, now_secs);
            true
        }
    }
}

/// Applies the product signing headers every AutoClaw proxy call needs.
fn apply_signed_headers(req: &mut UpstreamRequest, profile: &SignProfile, raw_token: &str) {
    let now = if profile.timestamp_millis {
        chrono::Utc::now().timestamp_millis().to_string()
    } else {
        openproxy_types::now_unix_secs_str()
    };
    let sign =
        crate::adapters::zai::md5::md5::md5_hex(format!("{}&{}&{}", APP_ID, now, profile.app_key));

    if let Ok(val) = crate::antigravity_headers::build_bearer_header(raw_token) {
        req.headers.insert(http::header::AUTHORIZATION, val);
    }
    req.headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static(CHECKIN_USER_AGENT),
    );
    req.headers.insert(
        http::HeaderName::from_static("x-auth-appid"),
        http::HeaderValue::from_static(APP_ID),
    );
    if let Ok(ts_val) = http::HeaderValue::from_str(&now) {
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
        http::HeaderValue::from_static(profile.client_type),
    );
    req.headers.insert(
        http::HeaderName::from_static("x-lang"),
        http::HeaderValue::from_static(profile.lang),
    );
    req.headers.insert(
        http::HeaderName::from_static("x-product"),
        http::HeaderValue::from_static("autoclaw"),
    );
    req.headers.insert(
        http::HeaderName::from_static("x-tm"),
        http::HeaderValue::from_static(profile.tm),
    );
    req.headers.insert(
        http::HeaderName::from_static("x-version"),
        http::HeaderValue::from_static(profile.version),
    );
}

/// GETs the task list. Read-only, so it is safe to use as a probe before
/// claiming anything.
async fn fetch_task_list(
    upstream: &Arc<UpstreamClient>,
    raw_token: &str,
    proxy: Option<&str>,
) -> Option<Value> {
    let mut req = UpstreamRequest::get(TASK_LIST_URL);
    req.proxy = proxy.map(ToString::to_string);
    apply_signed_headers(&mut req, &TASK_PROFILE, raw_token);

    let resp = upstream
        .call(req, TimeoutProfile::Quota, CancellationToken::new())
        .await
        .ok()?;
    if !resp.status.is_success() {
        tracing::debug!(
            status = resp.status.as_u16(),
            "autoclaw task list probe returned non-success"
        );
        return None;
    }
    let body = resp.collect().await.ok()?;
    serde_json::from_slice::<Value>(&body).ok()
}

/// Locates the `daily_signin` entry inside a task-list payload.
fn find_daily_signin(list: &Value) -> Option<&Value> {
    list.get("data")?
        .as_array()?
        .iter()
        .find(|t| t.get("task_id").and_then(|id| id.as_str()) == Some(DAILY_SIGNIN_TASK))
}

/// `true` when the upstream has today's daily sign-in already recorded.
fn daily_signin_completed(task: &Value) -> bool {
    task.get("status").and_then(|s| s.as_str()) == Some("completed")
}

/// Extracts the streak length out of a free-form `status_description`.
///
/// The upstream localises the string ("已签到 3 天"), so the only stable signal
/// is the first integer it contains.
fn parse_streak(description: &str) -> u32 {
    for word in description.split(|c: char| !c.is_ascii_digit()) {
        if let Ok(n) = word.parse::<u32>() {
            return n;
        }
    }
    0
}

fn streak_of(task: &Value) -> u32 {
    task.get("status_description")
        .and_then(|d| d.as_str())
        .map_or(0, parse_streak)
}

/// Claims one client-triggerable task. The response body is deliberately
/// ignored: the endpoint is idempotent and the authoritative state is re-read
/// from the task list afterwards.
async fn claim_task(
    upstream: &Arc<UpstreamClient>,
    raw_token: &str,
    proxy: Option<&str>,
    task_id: &str,
) {
    let body_bytes = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({ "task_id": task_id })).unwrap_or_default(),
    );
    let mut req = UpstreamRequest::post_json(TASK_COMPLETE_URL, body_bytes);
    req.proxy = proxy.map(ToString::to_string);
    apply_signed_headers(&mut req, &TASK_PROFILE, raw_token);
    let trace = uuid::Uuid::new_v4().to_string();
    if let Ok(trace_val) = http::HeaderValue::from_str(&trace) {
        req.headers
            .insert(http::HeaderName::from_static("x-trace-id"), trace_val);
    }

    match upstream
        .call(req, TimeoutProfile::Quota, CancellationToken::new())
        .await
    {
        Ok(resp) if !resp.status.is_success() => tracing::debug!(
            task_id,
            status = resp.status.as_u16(),
            "autoclaw task claim rejected by upstream"
        ),
        Ok(_) => tracing::debug!(task_id, "autoclaw task claim accepted"),
        Err(e) => tracing::debug!(task_id, error = %e, "autoclaw task claim failed"),
    }
}

/// Invokes `/userapi/v1/user-info` with the desktop signing profile.
///
/// In the official AutoClaw desktop client this endpoint triggers the daily and
/// weekly credit grants (`AutoClaw Weekly Perks +4000 credits`) that the task
/// endpoints never award.
async fn claim_autoclaw_user_info_credits(
    upstream: &Arc<UpstreamClient>,
    raw_token: &str,
    proxy: Option<&str>,
) {
    let device_id = extract_device_id_from_jwt(raw_token);
    let body_bytes = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "source_id": "autoclaw",
            "device_id": device_id
        }))
        .unwrap_or_default(),
    );

    let mut req = UpstreamRequest::post_json(USER_INFO_URL, body_bytes);
    req.proxy = proxy.map(ToString::to_string);
    apply_signed_headers(&mut req, &USER_INFO_PROFILE, raw_token);

    match upstream
        .call(req, TimeoutProfile::Quota, CancellationToken::new())
        .await
    {
        Ok(resp) if !resp.status.is_success() => tracing::debug!(
            status = resp.status.as_u16(),
            "autoclaw user-info perks claim rejected by upstream"
        ),
        Ok(_) => tracing::debug!("autoclaw user-info perks claim accepted"),
        Err(e) => tracing::debug!(error = %e, "autoclaw user-info perks claim failed"),
    }
}

fn extract_device_id_from_jwt(token: &str) -> String {
    let clean = token.trim();
    let parts: Vec<&str> = clean.split('.').collect();
    if parts.len() >= 2 {
        let payload_b64 = parts[1];
        let rem = payload_b64.len() % 4;
        let mut padded = payload_b64.to_string();
        if rem > 0 {
            padded.push_str(&"=".repeat(4 - rem));
        }
        if let Ok(bytes) =
            base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE, &padded)
            && let Ok(val) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && let Some(dev) = val.get("device_id").and_then(|d| d.as_str())
            && !dev.is_empty()
        {
            return dev.to_string();
        }
    }
    "b1b01c10-91dc-4af5-9d32-d17b5f5bbd9b".to_string()
}

/// Runs the daily check-in for one AutoClaw account.
///
/// This is the only place in the codebase that performs it. The quota fetch
/// must stay a pure read, so the caller invokes this from the quota-sync path
/// (`openproxy_core::quota_sync`), never from inside the balance fetch.
///
/// Returns `None` when the outcome could not be established, which means the
/// caller must persist nothing: a missing result is a signal, not a success.
pub async fn perform_autoclaw_checkin(
    upstream: &std::sync::Arc<UpstreamClient>,
    token: &str,
    proxy: Option<&str>,
) -> Option<AutoclawCheckinResult> {
    let now_u64 = openproxy_types::now_unix_secs_str()
        .parse::<u64>()
        .unwrap_or(0);
    if !checkin_dedupe_allows(token, now_u64) {
        tracing::debug!("autoclaw check-in skipped: a run for this credential is already recent");
        return None;
    }

    let clean_token = token.trim();
    let raw_token = clean_token
        .strip_prefix("Bearer ")
        .unwrap_or(clean_token)
        .to_string();
    if raw_token.is_empty() {
        tracing::warn!("autoclaw check-in skipped: empty credential");
        return None;
    }
    let raw_token = raw_token.as_str();

    // Phase 1 — read-only probe. The task list is the authority on whether
    // today's sign-in is already recorded, so no date arithmetic and no
    // in-process state can wrongly suppress it or repeat it.
    let probe = fetch_task_list(upstream, raw_token, proxy).await;
    match probe.as_ref().and_then(find_daily_signin) {
        Some(task) if daily_signin_completed(task) => {
            let streak = streak_of(task);
            tracing::info!(
                streak,
                "autoclaw daily check-in already recorded upstream; no claim issued"
            );
            return Some(AutoclawCheckinResult {
                last_checkin_date: today_utc(),
                streak_days: streak,
                already_claimed: true,
            });
        }
        Some(_) => {
            tracing::info!("autoclaw daily check-in pending upstream; claiming");
        }
        None => {
            // The probe failed. The claims are idempotent, so attempting them
            // is safe and is the only way to recover a lost day.
            tracing::warn!(
                "autoclaw task list probe returned no daily_signin; attempting claims anyway"
            );
        }
    }

    // Phase 2 — claim. Same ordering as the official desktop client: the perks
    // endpoint first, then the task burst.
    claim_autoclaw_user_info_credits(upstream, raw_token, proxy).await;
    tokio::time::sleep(CLAIM_SPACING).await;

    for (idx, task_id) in CLAIMABLE_TASKS.iter().enumerate() {
        if idx > 0 {
            tokio::time::sleep(CLAIM_SPACING).await;
        }
        claim_task(upstream, raw_token, proxy, task_id).await;
    }

    // Phase 3 — re-read the task list for the resulting streak. Without this
    // the streak is unknown and there is nothing trustworthy to record.
    tokio::time::sleep(CLAIM_SPACING).await;
    let after = fetch_task_list(upstream, raw_token, proxy).await;
    let Some(task) = after.as_ref().and_then(find_daily_signin) else {
        tracing::warn!(
            "autoclaw claims issued but the task list could not be re-read; streak not recorded"
        );
        return None;
    };
    let streak = streak_of(task);
    tracing::info!(streak, "autoclaw daily check-in claimed");
    Some(AutoclawCheckinResult {
        last_checkin_date: today_utc(),
        streak_days: streak,
        already_claimed: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_list(daily: Value) -> Value {
        serde_json::json!({ "data": [daily] })
    }

    #[test]
    fn completed_daily_signin_is_detected() {
        let list = task_list(serde_json::json!({
            "task_id": "daily_signin",
            "status": "completed",
            "status_description": "已签到 3 天"
        }));
        let task = find_daily_signin(&list).expect("daily_signin entry");
        assert!(daily_signin_completed(task));
        assert_eq!(streak_of(task), 3);
    }

    #[test]
    fn incomplete_daily_signin_is_not_completed() {
        let list = task_list(serde_json::json!({
            "task_id": "daily_signin",
            "status": "incomplete",
            "status_description": ""
        }));
        let task = find_daily_signin(&list).expect("daily_signin entry");
        assert!(!daily_signin_completed(task));
        assert_eq!(streak_of(task), 0);
    }

    #[test]
    fn missing_daily_signin_entry_yields_none() {
        let list = task_list(serde_json::json!({ "task_id": "download_pc_app" }));
        assert!(find_daily_signin(&list).is_none());
    }

    #[test]
    fn malformed_payload_yields_none_instead_of_panicking() {
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"data": null}),
            serde_json::json!({"data": "not-an-array"}),
            serde_json::json!({"data": [{"task_id": 7}]}),
        ] {
            assert!(find_daily_signin(&bad).is_none(), "payload {bad}");
        }
    }

    #[test]
    fn streak_parsing_takes_the_first_integer() {
        assert_eq!(parse_streak("已签到 12 天"), 12);
        assert_eq!(parse_streak("Checked in for 5 days"), 5);
        assert_eq!(parse_streak("no digits here"), 0);
        assert_eq!(parse_streak(""), 0);
    }

    #[test]
    fn today_utc_is_an_iso_date() {
        let today = today_utc();
        assert_eq!(today.len(), 10);
        assert_eq!(&today[4..5], "-");
        assert_eq!(&today[7..8], "-");
    }

    #[test]
    fn dedupe_window_blocks_a_second_run_but_allows_the_next_one() {
        let token = "Bearer test-credential";
        let start = 1_800_000_000_u64;
        assert!(checkin_dedupe_allows(token, start));
        assert!(!checkin_dedupe_allows(token, start + 30));
        assert!(!checkin_dedupe_allows(
            token,
            start + CHECKIN_DEDUPE_WINDOW_SECS - 1
        ));
        assert!(checkin_dedupe_allows(
            token,
            start + CHECKIN_DEDUPE_WINDOW_SECS
        ));
    }

    #[test]
    fn dedupe_is_scoped_per_credential() {
        let start = 1_800_000_000_u64;
        assert!(checkin_dedupe_allows("Bearer account-a", start));
        assert!(checkin_dedupe_allows("Bearer account-b", start));
        assert!(!checkin_dedupe_allows("Bearer account-a", start + 1));
    }
}
