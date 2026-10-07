use super::apply_codebuddy_spoofing_headers;
use crate::adapters::{
    Arc, CancellationToken, CoreError, Result, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use openproxy_types::quota::{AccountQuota, ModelQuotaDetail, now_unix_secs_str};

pub const CODEBUDDY_ACCOUNTS_URL: &str = "https://www.codebuddy.ai/v2/accounts";

pub const CODEBUDDY_GET_USER_RESOURCE_URL: &str =
    "https://www.codebuddy.ai/billing/meter/get-user-resource";

pub const CODEBUDDY_GET_USER_RESOURCE_SUMMARY_URL: &str =
    "https://www.codebuddy.ai/billing/meter/get-user-resource-summary";

pub const DEFAULT_CODEBUDDY_BASE_URL: &str = "https://www.codebuddy.ai/v2";

pub const CN_CODEBUDDY_BASE_URL: &str = "https://www.codebuddy.cn/v2";

pub const MIRROR_CODEBUDDY_BASE_URL: &str = "https://copilot.tencent.com/v2";

pub const LEGACY_CODEBUDDY_BASE_URL: &str = "https://www.codebuddy.ai/v2";

/// `OPENPROXY_CODEBUDDY_BASE_URL`, then `OPENPROXY_CODEBUDDY_AUTH_BASE_URL`,
/// then [`DEFAULT_CODEBUDDY_BASE_URL`].
#[must_use]
pub fn codebuddy_base_url() -> String {
    std::env::var("OPENPROXY_CODEBUDDY_BASE_URL")
        .or_else(|_| std::env::var("OPENPROXY_CODEBUDDY_AUTH_BASE_URL"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CODEBUDDY_BASE_URL.to_string())
}

/// Strips the `/v2` suffix, e.g. `https://www.codebuddy.ai/v2` ->
/// `https://www.codebuddy.ai`.
#[must_use]
pub fn codebuddy_origin_from_base_url(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if let Some(prefix) = trimmed.strip_suffix("/v2") {
        prefix.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Candidate origins in fallback order: configured, then `.ai`, `.cn` and
/// `copilot.tencent.com`. A localhost base URL suppresses the fallbacks.
#[must_use]
pub fn codebuddy_candidate_origins() -> Vec<String> {
    let configured_origin = codebuddy_origin_from_base_url(&codebuddy_base_url());
    let mut origins = vec![configured_origin.clone()];

    if configured_origin.contains("127.0.0.1") || configured_origin.contains("localhost") {
        return origins;
    }

    for fallback in [
        "https://www.codebuddy.ai",
        "https://www.codebuddy.cn",
        "https://copilot.tencent.com",
    ] {
        if !origins.iter().any(|o| o == fallback) {
            origins.push(fallback.to_string());
        }
    }

    origins
}

#[must_use]
pub fn parse_codebuddy_provider_specific(
    provider_specific: Option<&str>,
) -> (Option<i64>, Option<i64>) {
    let Some(raw) = provider_specific else {
        return (None, None);
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(raw) else {
        return (None, None);
    };
    let total = json
        .get("total_credits")
        .and_then(serde_json::Value::as_i64);
    let balance = json
        .get("credit_balance")
        .and_then(serde_json::Value::as_i64);
    (balance, total)
}

#[must_use]
pub fn is_codebuddy_auth_error(json: &serde_json::Value) -> bool {
    let code = json
        .get("code")
        .and_then(serde_json::Value::as_i64)
        .or_else(|| {
            json.get("response")
                .and_then(|r| r.get("code").or_else(|| r.pointer("/data/code")))
                .and_then(serde_json::Value::as_i64)
        });

    if let Some(c) = code
        && matches!(c, 14015 | 11140 | 11142 | 10001)
    {
        return true;
    }

    if let Some(msg) = json
        .get("msg")
        .or_else(|| json.get("message"))
        .and_then(serde_json::Value::as_str)
    {
        let lower = msg.to_ascii_lowercase();
        if lower.contains("token expired")
            || lower.contains("invalid token")
            || lower.contains("unauthorized")
            || lower.contains("refreshtoken is empty")
        {
            return true;
        }
    }

    false
}

pub use super::{CODEBUDDY_MODELS, CodeBuddyModelDef};

pub type CodeBuddyModelCreditCost = CodeBuddyModelDef;

pub const CODEBUDDY_MODEL_CREDIT_COSTS: &[CodeBuddyModelDef] = CODEBUDDY_MODELS;

/// Unix seconds of the next 00:00 in UTC+8, Tencent CodeBuddy's daily quota
/// reset boundary.
#[must_use]
pub fn calculate_next_midnight_cst_unix_secs() -> u64 {
    let now_utc = chrono::Utc::now();
    let Some(cst) = chrono::FixedOffset::east_opt(8 * 3600) else {
        return (now_utc.timestamp() + 86_400).max(0) as u64;
    };
    let now_cst = now_utc.with_timezone(&cst);
    let tomorrow_cst = now_cst
        .date_naive()
        .succ_opt()
        .unwrap_or(now_cst.date_naive());
    let next_midnight_naive = tomorrow_cst.and_hms_opt(0, 0, 0).unwrap_or_default();
    if let Some(cst_dt) = next_midnight_naive.and_local_timezone(cst).single() {
        cst_dt.to_utc().timestamp().max(0) as u64
    } else {
        (now_utc.timestamp() + 86_400).max(0) as u64
    }
}

#[must_use]
pub fn build_codebuddy_quota_model_details(
    session_limit: i64,
    session_used: i64,
    reset_at: Option<&str>,
) -> Vec<ModelQuotaDetail> {
    let mut details = Vec::with_capacity(CODEBUDDY_MODELS.len());

    for def in CODEBUDDY_MODELS {
        let (model_limit, model_used, rem_frac) = if def.credit_cost > 0.0 {
            let m_limit = ((session_limit as f64) / def.credit_cost).floor() as i64;
            let m_used = ((session_used as f64) / def.credit_cost).floor() as i64;
            let frac = if m_limit > 0 {
                ((m_limit - m_used) as f64 / m_limit as f64).clamp(0.0, 1.0)
            } else {
                0.0
            };
            (m_limit, m_used, frac)
        } else {
            // 0.00 credit cost, e.g. hy3.
            (9_999, 0, 1.0)
        };

        details.push(ModelQuotaDetail {
            model_id: def.id.to_string(),
            session_used: model_used,
            session_limit: model_limit,
            session_reset_at: reset_at.map(ToString::to_string),
            remaining_fraction: rem_frac,
        });
    }

    details
}

fn extract_numeric_field(val: &serde_json::Value, keys: &[&str]) -> Option<f64> {
    for key in keys {
        if let Some(v) = val.get(*key) {
            if let Some(n) = v.as_f64() {
                return Some(n);
            }
            if let Some(s) = v.as_str()
                && let Ok(n) = s.trim().replace(',', "").parse::<f64>()
            {
                return Some(n);
            }
        }
    }
    None
}

#[must_use]
pub fn parse_cst_datetime_to_unix_secs(s: &str) -> Option<u64> {
    let naive = chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S").ok()?;
    let cst = chrono::FixedOffset::east_opt(8 * 3600)?;
    let cst_dt = naive.and_local_timezone(cst).single()?;
    let ts = cst_dt.to_utc().timestamp();
    if ts > 0 { Some(ts as u64) } else { None }
}

/// [`AccountQuota`] from a `get-user-resource` or `get-user-resource-summary`
/// payload.
#[must_use]
pub fn parse_codebuddy_resource_quota(val: &serde_json::Value) -> Option<AccountQuota> {
    let now_utc = chrono::Utc::now().timestamp().max(0) as u64;
    parse_codebuddy_resource_quota_at(val, now_utc)
}

/// [`AccountQuota`] from a `get-user-resource` or `get-user-resource-summary`
/// payload evaluated at reference timestamp `now_utc`.
#[must_use]
pub fn parse_codebuddy_resource_quota_at(val: &serde_json::Value, now_utc: u64) -> Option<AccountQuota> {
    let mut total_capacity = 0i64;
    let mut total_remain = 0i64;
    let mut total_used = 0i64;
    let mut packages_info: Vec<(String, i64)> = Vec::new();
    let mut candidate_resets: Vec<u64> = Vec::new();

    let accounts = val
        .pointer("/data/Response/Data/Accounts")
        .or_else(|| val.pointer("/Response/Data/Accounts"))
        .or_else(|| val.pointer("/data/Accounts"))
        .or_else(|| val.pointer("/Accounts"))
        .and_then(serde_json::Value::as_array);

    let packages = val
        .pointer("/data/Packages")
        .or_else(|| val.pointer("/Packages"))
        .and_then(serde_json::Value::as_array);

    let accounts_or_packages_present = accounts.is_some() || packages.is_some();

    if let Some(accounts) = accounts {
        for acc in accounts {
            let status = acc
                .get("Status")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            if status != 0 {
                continue;
            }
            let size = extract_numeric_field(
                acc,
                &[
                    "CapacitySize",
                    "CycleCapacitySize",
                    "CapacitySizePrecise",
                    "CycleCapacitySizePrecise",
                ],
            )
            .unwrap_or(0.0)
            .round() as i64;

            let remain = extract_numeric_field(
                acc,
                &[
                    "CapacityRemain",
                    "CycleCapacityRemain",
                    "CapacityRemainPrecise",
                    "CycleCapacityRemainPrecise",
                ],
            )
            .unwrap_or(0.0)
            .round() as i64;

            let used = extract_numeric_field(
                acc,
                &[
                    "CapacityUsed",
                    "CycleCapacityUsed",
                    "CapacityUsedPrecise",
                    "CycleCapacityUsedPrecise",
                ],
            )
            .unwrap_or(0.0)
            .round() as i64;

            let pkg_name = acc
                .get("PackageName")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("Resource Package");

            total_capacity += size;
            total_remain += remain;
            total_used += used;

            packages_info.push((pkg_name.to_string(), size));

            if let Some(end_str) = acc.get("CycleEndTime").and_then(serde_json::Value::as_str)
                && let Some(ts) = parse_cst_datetime_to_unix_secs(end_str)
                && ts > now_utc
            {
                candidate_resets.push(ts);
            }
        }
    } else if let Some(packages) = packages {
        for pkg in packages {
            let total = extract_numeric_field(pkg, &["CycleTotalCapacity", "TotalCount"])
                .unwrap_or(0.0)
                .round() as i64;
            let remain = extract_numeric_field(pkg, &["CycleRemainCapacity"])
                .unwrap_or(0.0)
                .round() as i64;
            let used = extract_numeric_field(pkg, &["CycleUsedCapacity"])
                .unwrap_or(0.0)
                .round() as i64;
            let pkg_name = pkg
                .get("PackageName")
                .or_else(|| pkg.get("PackageCode"))
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("Package");

            total_capacity += total;
            total_remain += remain;
            total_used += used;

            packages_info.push((pkg_name.to_string(), total));
        }
    }

    if let Some(total_dosage) = val
        .pointer("/data/Response/Data/TotalDosage")
        .or_else(|| val.pointer("/Response/Data/TotalDosage"))
        .or_else(|| val.pointer("/data/TotalDosage"))
        .or_else(|| val.pointer("/TotalDosage"))
        .and_then(|v| extract_numeric_field(v, &[]).or_else(|| v.as_i64().map(|n| n as f64)))
    {
        let dosage_int = total_dosage.round() as i64;
        if dosage_int > 0 {
            total_capacity = dosage_int;
        }
    }

    if total_capacity <= 0 && !accounts_or_packages_present {
        return None;
    }

    let plan_name = if !packages_info.is_empty() {
        let parts: Vec<String> = packages_info
            .iter()
            .map(|(name, size)| format!("{name} ({size} credits)"))
            .collect();
        format!("CodeBuddy: {}", parts.join(" + "))
    } else {
        format!("CodeBuddy ({total_capacity} credits)")
    };

    let effective_used = if total_used > 0 {
        total_used
    } else {
        (total_capacity - total_remain).max(0)
    };

    let reset_at_secs = candidate_resets.into_iter().min();
    let reset_at_str = reset_at_secs.map(|ts| ts.to_string());
    let model_details = build_codebuddy_quota_model_details(
        total_capacity,
        effective_used,
        reset_at_str.as_deref(),
    );

    Some(AccountQuota {
        session_used: Some(effective_used),
        session_limit: Some(total_capacity),
        session_reset_at: reset_at_str,
        weekly_used: None,
        weekly_limit: None,
        weekly_reset_at: None,
        plan_name: Some(plan_name),
        last_fetched_at: now_unix_secs_str(),
        fetch_error: None,
        model_details: Some(model_details.into_boxed_slice()),
    })
}

#[must_use]
pub fn parse_codebuddy_accounts_quota(val: &serde_json::Value) -> AccountQuota {
    if let Some(quota) = parse_codebuddy_resource_quota(val) {
        return quota;
    }

    let accounts_arr = val
        .get("data")
        .and_then(|d| d.get("accounts"))
        .and_then(serde_json::Value::as_array)
        .or_else(|| val.get("accounts").and_then(serde_json::Value::as_array));

    let first_account = accounts_arr
        .and_then(|arr| {
            arr.iter().find(|acc| {
                acc.get("pluginEnabled")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true)
            })
        })
        .or_else(|| accounts_arr.and_then(|arr| arr.first()))
        .or_else(|| val.get("data"));

    let mut plan_name = "CodeBuddy Account".to_string();
    let mut session_limit = 0i64;
    let mut session_used = 0i64;

    if let Some(acc) = first_account {
        if let Some(account_type) = acc.get("type").and_then(serde_json::Value::as_str) {
            let t = account_type.trim().to_lowercase();
            if t == "enterprise" {
                plan_name = "CodeBuddy Enterprise".to_string();
            } else if t == "team" {
                plan_name = "CodeBuddy Team".to_string();
            } else if let Some(plan_str) = acc
                .get("plan")
                .or_else(|| acc.get("planName"))
                .and_then(serde_json::Value::as_str)
            {
                let p = plan_str.trim();
                if !p.is_empty() {
                    plan_name = format!("CodeBuddy {p}");
                }
            }
        }

        let parsed_limit = extract_numeric_field(
            acc,
            &[
                "total_credits",
                "credit_limit",
                "initial_credits",
                "total",
                "daily_credits",
            ],
        );
        if let Some(l) = parsed_limit
            && l > 0.0
        {
            session_limit = l.round() as i64;
        }

        let parsed_rem = extract_numeric_field(
            acc,
            &[
                "credits",
                "credit_balance",
                "remaining_credits",
                "remains",
                "balance",
                "remaining",
            ],
        );
        if let Some(rem) = parsed_rem {
            let rem_clamped = rem.max(0.0);
            session_used = (session_limit - rem_clamped.round() as i64).max(0);
        }
    }

    let model_details = build_codebuddy_quota_model_details(session_limit, session_used, None);

    AccountQuota {
        session_used: Some(session_used),
        session_limit: Some(session_limit),
        session_reset_at: None,
        weekly_used: None,
        weekly_limit: None,
        weekly_reset_at: None,
        plan_name: Some(plan_name),
        last_fetched_at: now_unix_secs_str(),
        fetch_error: None,
        model_details: Some(model_details.into_boxed_slice()),
    }
}

#[must_use]
pub fn build_codebuddy_resource_request(
    url: &str,
    token: &str,
    proxy_url: Option<&str>,
) -> UpstreamRequest {
    let mut req = UpstreamRequest::post_json(url, bytes::Bytes::from_static(b"{}"));
    req.proxy = proxy_url.map(ToString::to_string);

    let trimmed = token.trim();
    let auth_val = if trimmed.starts_with("Bearer ") {
        trimmed.to_string()
    } else {
        format!("Bearer {trimmed}")
    };
    if let Ok(hv) = http::HeaderValue::from_str(&auth_val) {
        req.headers.insert(http::header::AUTHORIZATION, hv);
    }
    req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json, text/plain, */*"),
    );
    if let Ok(uri) = url.parse::<http::Uri>()
        && let Some(host) = uri.host()
        && let Ok(val) = http::HeaderValue::from_str(host)
    {
        req.headers
            .insert(http::HeaderName::from_static("x-domain"), val);
    }

    apply_codebuddy_spoofing_headers(&mut req);
    req
}

#[must_use]
pub fn build_codebuddy_accounts_request_with_url(
    url: &str,
    token: &str,
    proxy_url: Option<&str>,
) -> UpstreamRequest {
    let mut req = UpstreamRequest::get(url);
    req.proxy = proxy_url.map(ToString::to_string);

    let trimmed = token.trim();
    let auth_val = if trimmed.starts_with("Bearer ") {
        trimmed.to_string()
    } else {
        format!("Bearer {trimmed}")
    };
    if let Ok(hv) = http::HeaderValue::from_str(&auth_val) {
        req.headers.insert(http::header::AUTHORIZATION, hv);
    }
    req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );
    req.headers.insert(
        http::HeaderName::from_static("x-no-enterprise-id"),
        http::HeaderValue::from_static("true"),
    );
    req.headers.insert(
        http::HeaderName::from_static("x-no-user-id"),
        http::HeaderValue::from_static("true"),
    );
    req.headers.insert(
        http::HeaderName::from_static("x-no-department-info"),
        http::HeaderValue::from_static("true"),
    );
    if let Ok(uri) = url.parse::<http::Uri>()
        && let Some(host) = uri.host()
        && let Ok(val) = http::HeaderValue::from_str(host)
    {
        req.headers
            .insert(http::HeaderName::from_static("x-domain"), val);
    }

    apply_codebuddy_spoofing_headers(&mut req);
    req
}

#[must_use]
pub fn build_codebuddy_accounts_request(token: &str, proxy_url: Option<&str>) -> UpstreamRequest {
    build_codebuddy_accounts_request_with_url(CODEBUDDY_ACCOUNTS_URL, token, proxy_url)
}

fn append_quota_error(dst: &mut String, err: &str) {
    if !dst.is_empty() {
        dst.push_str("; ");
    }
    dst.push_str(err);
}

pub async fn fetch_codebuddy_quota_unified(
    upstream: &Arc<UpstreamClient>,
    token: &str,
    provider_specific: Option<&str>,
    proxy_url: Option<&str>,
) -> Result<AccountQuota> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return Err(CoreError::Validation(
            "missing OAuth access token for codebuddy quota".into(),
        ));
    }

    let candidate_origins = codebuddy_candidate_origins();
    let mut last_err = String::new();

    for origin in &candidate_origins {
        let resource_url = format!("{origin}/billing/meter/get-user-resource");
        let summary_url = format!("{origin}/billing/meter/get-user-resource-summary");
        let accounts_url = format!("{origin}/v2/accounts");

        for meter_url in [&resource_url, &summary_url] {
            let req = build_codebuddy_resource_request(meter_url, trimmed, proxy_url);
            let cancel = CancellationToken::new();
            match upstream.call(req, TimeoutProfile::Quota, cancel).await {
                Ok(response) => {
                    let status = response.status;
                    if status == http::StatusCode::UNAUTHORIZED {
                        return Err(CoreError::UpstreamConnection(format!(
                            "{meter_url}: HTTP status 401 (token expired)"
                        )));
                    }
                    if status.is_success()
                        && let Ok(body) = response.collect().await
                        && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body)
                    {
                        if is_codebuddy_auth_error(&json) {
                            return Err(CoreError::UpstreamConnection(format!(
                                "{meter_url}: HTTP status 401 (token expired)"
                            )));
                        }
                        if let Some(quota) = parse_codebuddy_resource_quota(&json) {
                            return Ok(quota);
                        }
                    }
                    append_quota_error(
                        &mut last_err,
                        &format!("{meter_url}: status {}", status.as_u16()),
                    );
                }
                Err(e) => {
                    append_quota_error(&mut last_err, &format!("{meter_url}: {e}"));
                }
            }
        }

        let req_accounts =
            build_codebuddy_accounts_request_with_url(&accounts_url, trimmed, proxy_url);
        let cancel = CancellationToken::new();
        match upstream
            .call(req_accounts, TimeoutProfile::Quota, cancel)
            .await
        {
            Ok(response) => {
                let status = response.status;
                if status == http::StatusCode::UNAUTHORIZED {
                    return Err(CoreError::UpstreamConnection(format!(
                        "{accounts_url}: HTTP status 401 (token expired)"
                    )));
                }
                if status.is_success()
                    && let Ok(body) = response.collect().await
                    && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body)
                {
                    if is_codebuddy_auth_error(&json) {
                        return Err(CoreError::UpstreamConnection(format!(
                            "{accounts_url}: HTTP status 401 (token expired)"
                        )));
                    }
                    let mut quota = parse_codebuddy_accounts_quota(&json);
                    if quota.session_limit.unwrap_or(0) == 0 {
                        let (cached_bal, cached_tot) =
                            parse_codebuddy_provider_specific(provider_specific);
                        if let Some(tot) = cached_tot
                            && tot > 0
                        {
                            let bal = cached_bal.unwrap_or(tot);
                            let used = (tot - bal).max(0);
                            quota.session_limit = Some(tot);
                            quota.session_used = Some(used);
                            let reset_at_secs = calculate_next_midnight_cst_unix_secs();
                            let reset_at_str = reset_at_secs.to_string();
                            quota.session_reset_at = Some(reset_at_str.clone());
                            quota.model_details = Some(
                                build_codebuddy_quota_model_details(tot, used, Some(&reset_at_str))
                                    .into_boxed_slice(),
                            );
                        }
                    }
                    return Ok(quota);
                }
                append_quota_error(
                    &mut last_err,
                    &format!("{accounts_url}: status {}", status.as_u16()),
                );
            }
            Err(e) => {
                append_quota_error(&mut last_err, &format!("{accounts_url}: {e}"));
            }
        }
    }

    // Last resort: cached credit balances from `provider_specific`.
    let (cached_bal, cached_tot) = parse_codebuddy_provider_specific(provider_specific);
    if let Some(tot) = cached_tot
        && tot > 0
    {
        let bal = cached_bal.unwrap_or(tot);
        let used = (tot - bal).max(0);
        let reset_at_secs = calculate_next_midnight_cst_unix_secs();
        let reset_at_str = reset_at_secs.to_string();
        let model_details = build_codebuddy_quota_model_details(tot, used, Some(&reset_at_str));

        return Ok(AccountQuota {
            session_used: Some(used),
            session_limit: Some(tot),
            session_reset_at: Some(reset_at_str),
            weekly_used: None,
            weekly_limit: None,
            weekly_reset_at: None,
            plan_name: Some(format!("CodeBuddy (cached: {tot} credits)")),
            last_fetched_at: now_unix_secs_str(),
            fetch_error: Some(format!(
                "upstream connection error: failed to fetch CodeBuddy quota from billing meter endpoints ({last_err})"
            )),
            model_details: Some(model_details.into_boxed_slice()),
        });
    }

    Err(CoreError::UpstreamConnection(if last_err.is_empty() {
        "failed to fetch CodeBuddy quota from billing meter endpoints".into()
    } else {
        format!("failed to fetch CodeBuddy quota from billing meter endpoints: {last_err}")
    }))
}

#[cfg(test)]
#[path = "quota_tests.rs"]
mod tests;
