use super::{ZAI_APP_VERSION, ZAI_BUSINESS_LOGIN_URL};
use crate::adapters::{
    Arc, CancellationToken, Result, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use openproxy_types::{AccountQuota, ModelQuotaDetail};

mod coding_plan;
mod dual;
#[cfg(test)]
mod dual_tests;
mod envelope;
mod zcode;

use dual::fetch_dual_quota;

#[derive(Default, Clone)]
pub struct ZaiTokens {
    pub zcode_jwt_token: Option<String>,
    pub business_access_token: Option<String>,
    pub zai_access_token: Option<String>,
    pub api_key: Option<String>,
}

impl ZaiTokens {
    pub fn from_provider_specific_and_args(
        api_key: &str,
        access_token: Option<&str>,
        provider_specific: Option<&str>,
    ) -> Self {
        let mut tokens = Self::default();

        if let Some(raw) = provider_specific
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(raw)
        {
            tokens.zcode_jwt_token = v
                .get("zcode_jwt_token")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string);

            tokens.business_access_token = v
                .get("business_access_token")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string);

            tokens.zai_access_token = v
                .get("zai_access_token")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string);

            tokens.api_key = v
                .get("api_key")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string);
        }

        // Metadata is authoritative. Never send an opaque business token to
        // ZCode, or a three-segment JWT to the Coding Plan key endpoint.
        for credential in [access_token.unwrap_or_default(), api_key] {
            let credential = credential.trim();
            if credential.is_empty()
                || [
                    tokens.zcode_jwt_token.as_deref(),
                    tokens.business_access_token.as_deref(),
                    tokens.zai_access_token.as_deref(),
                    tokens.api_key.as_deref(),
                ]
                .contains(&Some(credential))
            {
                continue;
            }
            let parts: Vec<_> = credential.split('.').collect();
            if parts.len() == 3 && parts.iter().all(|part| !part.is_empty()) {
                tokens
                    .zcode_jwt_token
                    .get_or_insert_with(|| credential.to_owned());
            } else {
                tokens.api_key.get_or_insert_with(|| credential.to_owned());
            }
        }

        tokens
    }
}

fn build_zai_get_request(url: &str, token: &str, proxy_url: Option<&str>) -> UpstreamRequest {
    let mut req = UpstreamRequest::get(url);
    req.proxy = proxy_url.map(ToString::to_string);
    if let Ok(val) = crate::antigravity_headers::build_bearer_header(token) {
        req.headers.insert(http::header::AUTHORIZATION, val);
    }
    req.headers.insert(
        http::header::ACCEPT,
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
    let device_mid = uuid::Uuid::new_v4().to_string();
    if let Ok(mid_val) = http::HeaderValue::from_str(&device_mid) {
        req.headers
            .insert(http::HeaderName::from_static("x-device-mid"), mid_val);
    }
    req
}

pub(crate) async fn exchange_business_token_on_the_fly(
    upstream: &Arc<UpstreamClient>,
    zai_access_token: &str,
    proxy_url: Option<&str>,
) -> Option<String> {
    let body = serde_json::json!({ "token": zai_access_token });
    let body_bytes = serde_json::to_vec(&body).ok()?;

    let mut req =
        UpstreamRequest::post_json(ZAI_BUSINESS_LOGIN_URL, bytes::Bytes::from(body_bytes));
    req.proxy = proxy_url.map(ToString::to_string);
    req.headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );

    let cancel = CancellationToken::new();
    let resp = upstream
        .call(req, TimeoutProfile::OAuth, cancel)
        .await
        .ok()?;
    if !resp.status.is_success() {
        return None;
    }

    let resp_bytes = resp.collect().await.ok()?;
    let json: serde_json::Value = serde_json::from_slice(&resp_bytes).ok()?;
    if !envelope::envelope_is_success(&json) {
        return None;
    }

    json.get("data")
        .and_then(|d| d.get("access_token").or_else(|| d.get("accessToken")))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

pub(crate) fn parse_zai_coding_plan_quota(
    sub_json: Option<&serde_json::Value>,
    limit_json: Option<&serde_json::Value>,
) -> Option<AccountQuota> {
    if sub_json.is_some_and(|v| !envelope::envelope_is_success(v))
        || limit_json.is_some_and(|v| !envelope::envelope_is_success(v))
    {
        return None;
    }
    let mut plan_name = None;
    let mut period_end_secs = None;

    if let Some(sub) = sub_json
        && let Some(data) = sub.get("data").and_then(serde_json::Value::as_array)
    {
        for s in data {
            let status = s
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let in_period = s
                .get("inCurrentPeriod")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);

            if status.eq_ignore_ascii_case("VALID") && in_period {
                let name = s
                    .get("productName")
                    .or_else(|| s.get("productId"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|n| !n.is_empty());
                if let Some(name) = name {
                    plan_name = Some(name.to_string());
                }
                if let Some(end_ts) = s.get("periodEndTime").and_then(serde_json::Value::as_u64) {
                    period_end_secs = Some(normalize_unix_secs(end_ts));
                }
                break;
            }
        }
    }

    let limit_data = limit_json.and_then(|j| {
        let code = j
            .get("code")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let success = j
            .get("success")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        if code != 0 && code != 200 || !success {
            return None;
        }
        j.get("data")
    });

    if limit_data.is_none() && plan_name.is_none() {
        return None;
    }

    let mut session_used = None;
    let mut session_limit = None;
    let mut session_reset_at = period_end_secs.map(|s| s.to_string());
    let mut model_details = Vec::new();

    if let Some(data) = limit_data {
        if plan_name.is_none()
            && let Some(lvl) = data.get("level").and_then(serde_json::Value::as_str)
        {
            let lvl = lvl.trim();
            if !lvl.is_empty() {
                plan_name = Some(format!("Z.ai Coding Plan ({lvl})"));
            }
        }

        if let Some(limits) = data.get("limits").and_then(serde_json::Value::as_array) {
            let primary = limits
                .iter()
                .find(|l| {
                    l.get("type")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|t| t.eq_ignore_ascii_case("TIME_LIMIT"))
                })
                .or_else(|| limits.iter().find(|l| l.get("remaining").is_some()))
                .or_else(|| limits.first());

            if let Some(p) = primary {
                let number = p
                    .get("number")
                    .or_else(|| p.get("unit"))
                    .and_then(serde_json::Value::as_i64);
                let usage = p
                    .get("usage")
                    .or_else(|| p.get("used"))
                    .or_else(|| p.get("currentValue"))
                    .and_then(serde_json::Value::as_i64);

                session_limit = number;
                session_used = usage;

                if let Some(rst) = p.get("nextResetTime").and_then(serde_json::Value::as_u64) {
                    session_reset_at = Some(normalize_unix_secs(rst).to_string());
                }

                if let Some(details) = p.get("usageDetails").and_then(serde_json::Value::as_array) {
                    for d in details {
                        let model_id = d
                            .get("modelCode")
                            .or_else(|| d.get("model"))
                            .or_else(|| d.get("displayName"))
                            .and_then(serde_json::Value::as_str)
                            .map_or("", str::trim);
                        if model_id.is_empty() {
                            continue;
                        }

                        let m_used = d
                            .get("usage")
                            .or_else(|| d.get("used"))
                            .and_then(serde_json::Value::as_i64)
                            .unwrap_or(0);
                        let m_limit = d
                            .get("number")
                            .or_else(|| d.get("limit"))
                            .and_then(serde_json::Value::as_i64)
                            .or(session_limit)
                            .unwrap_or(0);

                        let rem_frac = if m_limit > 0 {
                            ((m_limit - m_used) as f64 / m_limit as f64).clamp(0.0, 1.0)
                        } else {
                            0.0
                        };

                        model_details.push(ModelQuotaDetail {
                            model_id: model_id.to_string(),
                            session_used: m_used,
                            session_limit: m_limit,
                            session_reset_at: session_reset_at.clone(),
                            remaining_fraction: rem_frac,
                        });
                    }
                }
            }
        }
    }

    if plan_name.is_none() {
        plan_name = Some("Z.ai Coding Plan".into());
    }

    Some(AccountQuota {
        session_used,
        session_limit,
        session_reset_at,
        weekly_used: None,
        weekly_limit: None,
        weekly_reset_at: None,
        plan_name,
        last_fetched_at: openproxy_types::now_unix_secs_str(),
        fetch_error: None,
        pools: None,
        model_details: if model_details.is_empty() {
            None
        } else {
            Some(model_details.into_boxed_slice())
        },
    })
}

pub(crate) async fn fetch_zai_quota_unified(
    upstream: &Arc<UpstreamClient>,
    api_key: &str,
    access_token: Option<&str>,
    provider_specific: Option<&str>,
    proxy_url: Option<&str>,
) -> Result<AccountQuota> {
    let mut tokens =
        ZaiTokens::from_provider_specific_and_args(api_key, access_token, provider_specific);

    if tokens.business_access_token.is_none()
        && let Some(zai_tok) = tokens.zai_access_token.as_deref()
    {
        tokens.business_access_token =
            exchange_business_token_on_the_fly(upstream, zai_tok, proxy_url).await;
    }

    fetch_dual_quota(upstream, &tokens, proxy_url).await
}

fn normalize_unix_secs(ts: u64) -> u64 {
    if ts > 100_000_000_000 { ts / 1000 } else { ts }
}
