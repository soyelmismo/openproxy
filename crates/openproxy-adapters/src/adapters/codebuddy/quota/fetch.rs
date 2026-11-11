//! Unified quota fetch: meter endpoints, accounts endpoint and the cached
//! provider-specific fallback.

use crate::adapters::{Arc, CancellationToken, CoreError, Result, TimeoutProfile, UpstreamClient};
use openproxy_types::quota::{AccountQuota, now_unix_secs_str};

use super::{
    build_codebuddy_accounts_request_with_url, build_codebuddy_quota_model_details,
    build_codebuddy_resource_request, calculate_next_midnight_cst_unix_secs,
    codebuddy_candidate_origins, is_codebuddy_auth_error, parse_codebuddy_accounts_quota,
    parse_codebuddy_provider_specific, parse_codebuddy_resource_quota,
};

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
            pools: None,
            model_details: Some(model_details.into_boxed_slice()),
        });
    }

    Err(CoreError::UpstreamConnection(if last_err.is_empty() {
        "failed to fetch CodeBuddy quota from billing meter endpoints".into()
    } else {
        format!("failed to fetch CodeBuddy quota from billing meter endpoints: {last_err}")
    }))
}
