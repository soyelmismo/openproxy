//! Z.ai route preparation shared by unary and streaming target dispatch.

use crate::Pipeline;
use crate::context::ResolvedTarget;
use openproxy_adapters::adapters::zai::{
    ZaiInferenceRoute, exhaust_zcode_model, refresh_zai_account_quota, resolve_zai_inference_route,
    zai_snapshot_from_account,
};
use openproxy_types::{Account, AccountQuota, CoreError, Result};

pub(crate) struct ZaiPreparedRoute {
    pub account: Account,
    pub route: ZaiInferenceRoute,
    fallback_credential: String,
}

async fn persist_snapshot(
    pipeline: &Pipeline,
    account: &Account,
    quota: AccountQuota,
) -> Result<()> {
    let account_id = account.id;
    let conn = std::sync::Arc::clone(&pipeline.conn);
    let pool = pipeline.db_pool.clone();
    let result = tokio::task::spawn_blocking(move || {
        if let Some(pool) = pool {
            openproxy_db::accounts::set_quota(&pool.writer(), account_id, &quota)
        } else {
            openproxy_db::accounts::set_quota(&conn.lock(), account_id, &quota)
        }
    })
    .await;
    result.map_err(|_| CoreError::Internal("Z.ai quota persistence task failed".into()))?
}

pub(crate) async fn prepare_zai_route(
    pipeline: &Pipeline,
    current: &ResolvedTarget,
    paid_base_url: &str,
    proxy_override: Option<&(String, String)>,
) -> Result<ZaiPreparedRoute> {
    let account_id = current.target.account_id.ok_or_else(|| {
        CoreError::Auth("Z.ai requires an account for quota-aware routing".into())
    })?;
    let key = std::sync::Arc::clone(&pipeline.config.master_key);
    let mut account = pipeline
        .async_repo()
        .run(move |repo| repo.get_account(account_id, &key))
        .await?
        .ok_or(CoreError::AccountNotFound(account_id.0))?;
    let fallback = current
        .custom_meta
        .as_ref()
        .map_or(current.api_key.as_str(), |meta| meta.access_token.as_str());
    let proxy = if let Some((_, url)) = proxy_override {
        Some(url.clone())
    } else {
        let provider = current.target.provider_id.clone();
        pipeline
            .async_repo()
            .run(move |repo| repo.get_or_assign_provider_proxy(&provider, Some(account_id)))
            .await?
    };
    let snapshot = refresh_zai_account_quota(
        &pipeline.config.upstream_client,
        &mut account,
        fallback,
        proxy.as_deref(),
        false,
    )
    .await?;
    if let Some(snapshot) = snapshot {
        persist_snapshot(pipeline, &account, snapshot).await?;
    }
    let now = openproxy_types::now_unix_secs_str()
        .parse::<u64>()
        .unwrap_or(0);
    let route = resolve_zai_inference_route(
        &account,
        fallback,
        current.model.model_id.as_str(),
        now,
        paid_base_url,
    )?;
    tracing::debug!(account_id = account_id.0, source = ?route.source, "selected Z.ai quota source");
    Ok(ZaiPreparedRoute {
        account,
        route,
        fallback_credential: fallback.to_owned(),
    })
}

/// The caller invokes this only for a structured server-confirmed depletion
/// before any successful response. There is no fallback after streaming starts.
pub(crate) async fn record_zai_route_failure(
    pipeline: &Pipeline,
    prepared: &mut ZaiPreparedRoute,
    model: &str,
    error: &CoreError,
) -> Result<()> {
    if !matches!(
        error,
        CoreError::UpstreamError { .. }
            | CoreError::RateLimited { .. }
            | CoreError::UpstreamTimeout { .. }
            | CoreError::UpstreamConnection(_)
    ) {
        return Ok(());
    }
    let exhausted = openproxy_adapters::adapters::zai::is_zcode_entitlement_exhaustion(error);
    if let Some(pools) = prepared.account.quota_pools.as_deref_mut() {
        for pool in pools
            .iter_mut()
            .filter(|p| p.source == prepared.route.source && p.matches_model(model))
        {
            if exhausted {
                pool.status = openproxy_types::quota::QuotaPoolStatus::Exhausted;
                pool.remaining = Some(0);
                pool.fetch_error = None;
            } else {
                pool.status = openproxy_types::quota::QuotaPoolStatus::Unavailable;
                pool.fetch_error = Some(format!(
                    "Selected quota inference failed (HTTP {}); balance requires refresh",
                    error.http_status()
                ));
            }
            pool.last_fetched_at = openproxy_types::now_unix_secs_str();
        }
    }
    persist_snapshot(
        pipeline,
        &prepared.account,
        zai_snapshot_from_account(&prepared.account),
    )
    .await
}

pub(crate) async fn prepare_zai_paid_fallback(
    pipeline: &Pipeline,
    prepared: &mut ZaiPreparedRoute,
    model: &str,
    paid_base_url: &str,
) -> Result<()> {
    exhaust_zcode_model(&mut prepared.account, model);
    let snapshot = zai_snapshot_from_account(&prepared.account);
    persist_snapshot(pipeline, &prepared.account, snapshot).await?;
    let now = openproxy_types::now_unix_secs_str()
        .parse::<u64>()
        .unwrap_or(0);
    let route = resolve_zai_inference_route(
        &prepared.account,
        &prepared.fallback_credential,
        model,
        now,
        paid_base_url,
    )?;
    if route.source != openproxy_types::quota::QuotaSource::CodingPlan {
        return Err(CoreError::Validation(
            "no independent Coding Plan quota is available".into(),
        ));
    }
    prepared.route = route;
    Ok(())
}

#[cfg(test)]
#[path = "zai_tests.rs"]
mod zai_tests;
