//! Refresh account quotas before choosing an independently authenticated route.

use super::fetch_zai_quota_unified;
use crate::upstream::UpstreamClient;
use openproxy_types::Result;
use openproxy_types::accounts::Account;
use openproxy_types::quota::{AccountQuota, QuotaPoolStatus, QuotaSource};
use std::sync::Arc;

const SNAPSHOT_MAX_AGE_SECS: u64 = 60;

pub fn apply_zai_quota_snapshot(account: &mut Account, quota: &AccountQuota) {
    account.quota_session_used = quota.session_used;
    account.quota_session_limit = quota.session_limit;
    account.quota_session_reset_at = quota.session_reset_at.clone().map(Into::into);
    account.quota_weekly_used = quota.weekly_used;
    account.quota_weekly_limit = quota.weekly_limit;
    account.quota_weekly_reset_at = quota.weekly_reset_at.clone().map(Into::into);
    account.quota_plan_name = quota.plan_name.clone().map(Into::into);
    account.quota_last_fetched_at = Some(quota.last_fetched_at.clone().into());
    account.quota_fetch_error = quota.fetch_error.clone().map(Into::into);
    account.quota_model_details = quota.model_details.clone();
    account.quota_pools = quota.pools.clone();
}

pub async fn refresh_zai_account_quota(
    upstream: &Arc<UpstreamClient>,
    account: &mut Account,
    fallback_credential: &str,

    proxy: Option<&str>,
    force_refresh: bool,
) -> Result<Option<AccountQuota>> {
    let now = openproxy_types::now_unix_secs_str()
        .parse::<u64>()
        .unwrap_or(0);
    let fresh = account.quota_pools.as_deref().is_some_and(|pools| {
        pools.iter().any(|p| p.source == QuotaSource::ZcodeStarter)
            && pools.iter().any(|p| p.source == QuotaSource::CodingPlan)
            && pools.iter().all(|pool| {
                pool.last_fetched_at.parse::<u64>().is_ok_and(|fetched| {
                    fetched <= now && now.saturating_sub(fetched) <= SNAPSHOT_MAX_AGE_SECS
                })
            })
    });
    if force_refresh || !fresh {
        let quota = fetch_zai_quota_unified(
            upstream,
            "",
            Some(fallback_credential),
            account.oauth_provider_specific.as_deref(),
            proxy,
        )
        .await?;
        let previous = zai_snapshot_from_account(account);
        let quota = quota.merge_over_previous(Some(&previous));
        apply_zai_quota_snapshot(account, &quota);
        Ok(Some(quota))
    } else {
        Ok(None)
    }
}

/// A server-confirmed depleted model must not be retried through another
/// bucket whose cached counter was fetched before the failed generation.
/// Other models and the independent paid pool remain untouched.
pub fn exhaust_zcode_model(account: &mut Account, model: &str) {
    if let Some(pools) = account.quota_pools.as_deref_mut() {
        for pool in pools
            .iter_mut()
            .filter(|pool| pool.source == QuotaSource::ZcodeStarter && pool.matches_model(model))
        {
            pool.status = QuotaPoolStatus::Exhausted;
            pool.remaining = Some(0);
        }
    }
}

pub fn zai_snapshot_from_account(account: &Account) -> AccountQuota {
    AccountQuota {
        session_used: account.quota_session_used,
        session_limit: account.quota_session_limit,
        session_reset_at: account
            .quota_session_reset_at
            .as_deref()
            .map(ToString::to_string),
        weekly_used: account.quota_weekly_used,
        weekly_limit: account.quota_weekly_limit,
        weekly_reset_at: account
            .quota_weekly_reset_at
            .as_deref()
            .map(ToString::to_string),
        plan_name: account.quota_plan_name.as_deref().map(ToString::to_string),
        last_fetched_at: openproxy_types::now_unix_secs_str(),
        fetch_error: account
            .quota_fetch_error
            .as_deref()
            .map(ToString::to_string),
        model_details: account.quota_model_details.clone(),
        pools: account.quota_pools.clone(),
    }
}
