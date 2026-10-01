//! openproxy-oauth: OAuth 2.0 multi-provider infrastructure, refresh scheduler,
//! device ticket management and credential scanning.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub(crate) use openproxy_types::error;
pub(crate) use openproxy_types::ids;

pub mod accounts {
    pub use openproxy_db::accounts::*;
    pub use openproxy_types::accounts::*;
}

pub mod providers {
    pub use openproxy_db::providers::*;
    pub use openproxy_types::providers::*;
}

pub use openproxy_notifications as notifications;

pub async fn fetch_account_quota_with_proxy(
    provider_id: &str,
    upstream: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
    api_key: &str,
    access_token: Option<&str>,
    provider_specific: Option<&str>,
    proxy_url: Option<&str>,
) -> openproxy_types::AccountQuota {
    let mut result_quota = None;

    let mapped_id = match provider_id {
        "minimax-cn" => "minimax",
        "agy" => "antigravity",
        "zcode" | "z.ai" => "zai",
        other => other,
    };

    let adapters = openproxy_adapters::adapters::builtin_adapters();
    if let Some(adapter) = adapters.iter().find(|a| a.id().as_str() == mapped_id)
        && let Some(res) = adapter
            .fetch_quota_with_proxy(
                upstream,
                api_key,
                access_token,
                provider_specific,
                proxy_url,
            )
            .await
    {
        result_quota =
            Some(res.unwrap_or_else(|e| openproxy_types::AccountQuota::with_error(e.to_string())));
    }

    result_quota.unwrap_or_else(|| {
        openproxy_types::AccountQuota::with_error(format!(
            "quota fetching not implemented for provider '{provider_id}'"
        ))
    })
}

pub async fn fetch_account_quota(
    provider_id: &str,
    upstream: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
    api_key: &str,
    access_token: Option<&str>,
    provider_specific: Option<&str>,
) -> openproxy_types::AccountQuota {
    fetch_account_quota_with_proxy(
        provider_id,
        upstream,
        api_key,
        access_token,
        provider_specific,
        None,
    )
    .await
}

pub mod admin {
    pub use crate::{fetch_account_quota, fetch_account_quota_with_proxy};
}

pub mod account_scanner;
pub use account_scanner::*;

pub mod oauth;
pub use oauth::*;
