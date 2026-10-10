//! Independent source fetches and sanitized failures. Never swap credentials.
use super::coding_plan::parse_coding_plan_pools;
use super::envelope::envelope_is_success;
use super::zcode::parse_zcode_pools;
use super::{ZaiTokens, build_zai_get_request};
use crate::adapters::{Arc, CancellationToken, CoreError, Result, TimeoutProfile, UpstreamClient};
use openproxy_types::quota::{AccountQuota, QuotaPool, QuotaPoolStatus, QuotaSource};
use serde_json::Value;

pub(super) struct QuotaEndpoints<'a> {
    pub(super) starter: &'a str,
    pub(super) subscriptions: &'a str,
    pub(super) limits: &'a str,
}
const ENDPOINTS: QuotaEndpoints<'static> = QuotaEndpoints {
    starter: "https://zcode.z.ai/api/v1/zcode-plan/billing/balance?app_version=3.14.0",
    subscriptions: super::super::ZAI_SUBSCRIPTION_LIST_URL,
    limits: super::super::ZAI_QUOTA_LIMIT_URL,
};

pub(crate) fn status_pool(
    source: QuotaSource,
    status: QuotaPoolStatus,
    error: Option<String>,
    now: u64,
) -> QuotaPool {
    QuotaPool {
        id: match source {
            QuotaSource::ZcodeStarter => "zcode-starter",
            QuotaSource::CodingPlan => "coding-plan",
        }
        .into(),
        source,
        plan_name: None,
        status,
        unit: match source {
            QuotaSource::ZcodeStarter => "token",
            QuotaSource::CodingPlan => "percentage",
        }
        .into(),
        used: None,
        limit: None,
        remaining: None,
        reset_at: None,
        expires_at: None,
        starts_at: None,
        model_ids: Vec::new(),
        model_details: None,
        fetch_error: error,
        last_fetched_at: now.to_string(),
    }
}

async fn fetch_json(
    upstream: &Arc<UpstreamClient>,
    url: &str,
    token: &str,
    proxy: Option<&str>,
) -> Result<Value> {
    let req = build_zai_get_request(url, token, proxy);
    let response = upstream
        .call(req, TimeoutProfile::Quota, CancellationToken::new())
        .await
        .map_err(|_| CoreError::UpstreamConnection("quota request failed (transport)".into()))?;
    let status = response.status;
    if !status.is_success() {
        // An upstream can echo Authorization in its body. Persist classification
        // only, not even a truncated fragment of that untrusted body.
        return Err(CoreError::UpstreamConnection(format!(
            "quota request failed (HTTP {})",
            status.as_u16()
        )));
    }
    let body = response
        .collect()
        .await
        .map_err(|_| CoreError::UpstreamConnection("quota response could not be read".into()))?;
    serde_json::from_slice(&body)
        .map_err(|_| CoreError::Parse("quota response is not valid JSON".into()))
}

async fn fetch_paid(
    upstream: &Arc<UpstreamClient>,
    token: &str,
    proxy: Option<&str>,
    now: u64,
    endpoints: &QuotaEndpoints<'_>,
) -> Result<Vec<QuotaPool>> {
    let subscriptions = fetch_json(upstream, endpoints.subscriptions, token, proxy).await?;
    if envelope_is_success(&subscriptions)
        && subscriptions
            .get("data")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
    {
        return Ok(vec![status_pool(
            QuotaSource::CodingPlan,
            QuotaPoolStatus::Absent,
            None,
            now,
        )]);
    }
    let limits = fetch_json(upstream, endpoints.limits, token, proxy).await?;
    parse_coding_plan_pools(&subscriptions, &limits, now)
}

fn source_result(result: Result<Vec<QuotaPool>>, source: QuotaSource, now: u64) -> Vec<QuotaPool> {
    match result {
        Ok(pools) if !pools.is_empty() => pools,
        Err(error) => {
            let class = match error {
                CoreError::UpstreamConnection(ref message)
                    if message.starts_with("quota request failed (HTTP ") =>
                {
                    let status = message
                        .strip_prefix("quota request failed (HTTP ")
                        .and_then(|v| v.strip_suffix(')'))
                        .and_then(|v| v.parse::<u16>().ok())
                        .filter(|v| (100..600).contains(v));
                    status.map_or_else(|| "transport".to_owned(), |s| format!("HTTP {s}"))
                }
                CoreError::UpstreamConnection(_) => "transport".into(),
                _ => "unreadable response or failed envelope".into(),
            };
            vec![status_pool(
                source,
                QuotaPoolStatus::Unavailable,
                Some(format!("{source:?} quota unavailable ({class})")),
                now,
            )]
        }
        Ok(_) => vec![status_pool(
            source,
            QuotaPoolStatus::Unavailable,
            Some("Quota response carried no source verdict".into()),
            now,
        )],
    }
}

pub(crate) async fn fetch_dual_quota(
    upstream: &Arc<UpstreamClient>,
    tokens: &ZaiTokens,
    proxy: Option<&str>,
) -> Result<AccountQuota> {
    fetch_dual_at(upstream, tokens, proxy, &ENDPOINTS).await
}

pub(super) async fn fetch_dual_at(
    upstream: &Arc<UpstreamClient>,
    tokens: &ZaiTokens,
    proxy: Option<&str>,
    endpoints: &QuotaEndpoints<'_>,
) -> Result<AccountQuota> {
    let now = openproxy_types::now_unix_secs_str()
        .parse::<u64>()
        .unwrap_or(0);
    let starter = async {
        match tokens.zcode_jwt_token.as_deref() {
            Some(token) => source_result(
                fetch_json(upstream, endpoints.starter, token, proxy)
                    .await
                    .and_then(|json| parse_zcode_pools(&json, now)),
                QuotaSource::ZcodeStarter,
                now,
            ),
            None => vec![status_pool(
                QuotaSource::ZcodeStarter,
                QuotaPoolStatus::Unavailable,
                Some("ZCode OAuth credential is missing; Starter balance is unknown".into()),
                now,
            )],
        }
    };
    let paid = async {
        match tokens
            .business_access_token
            .as_deref()
            .or(tokens.api_key.as_deref())
        {
            Some(token) => source_result(
                fetch_paid(upstream, token, proxy, now, endpoints).await,
                QuotaSource::CodingPlan,
                now,
            ),
            None => vec![status_pool(
                QuotaSource::CodingPlan,
                QuotaPoolStatus::Unavailable,
                Some(
                    "Coding Plan credential is missing; subscription could not be verified".into(),
                ),
                now,
            )],
        }
    };
    let (mut pools, paid) = tokio::join!(starter, paid);
    pools.extend(paid);
    Ok(AccountQuota {
        session_used: None,
        session_limit: None,
        session_reset_at: None,
        weekly_used: None,
        weekly_limit: None,
        weekly_reset_at: None,
        plan_name: None,
        last_fetched_at: now.to_string(),
        fetch_error: None,
        model_details: None,
        pools: Some(pools.into_boxed_slice()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn untrusted_failure_never_leaks_upstream_or_becomes_zero() {
        let result = source_result(
            Err(CoreError::Parse(
                "upstream echoed private credential".into(),
            )),
            QuotaSource::ZcodeStarter,
            1000,
        );
        assert_eq!(result[0].status, QuotaPoolStatus::Unavailable);
        assert_eq!(result[0].remaining, None);
        assert!(
            !result[0]
                .fetch_error
                .as_deref()
                .unwrap()
                .contains("private credential")
        );
    }
}
