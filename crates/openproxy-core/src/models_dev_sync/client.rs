//! Upstream HTTP fetching, one-shot triggers, and periodic sync scheduler.

use super::backfill::recompute_costs;
use super::combos::auto_create_combos;
use super::enrich::enrich_models_from_sync;
use super::provider_map::MODELS_DEV_URL;
use super::upsert::upsert_models_dev;
use crate::error::{CoreError, Result};
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use std::sync::Arc;

async fn handle_fetch_attempt_error(
    attempt: u32,
    max_retries: u32,
    backoff: &mut std::time::Duration,
    e: CoreError,
) -> Result<()> {
    if attempt == max_retries {
        tracing::warn!(attempt, error = %e, "models.dev fetch failed after all retries");
        Err(e)
    } else {
        tracing::warn!(
            attempt,
            next_backoff_ms = backoff.as_millis() as u64,
            error = %e,
            "models.dev fetch failed; retrying"
        );
        tokio::time::sleep(*backoff).await;
        *backoff *= 2;
        Ok(())
    }
}

/// Fetch raw JSON bytes from models.dev with retries.
pub async fn fetch_models_dev(upstream: &Arc<UpstreamClient>) -> Result<bytes::Bytes> {
    const MAX_RETRIES: u32 = 3;
    let mut backoff = std::time::Duration::from_secs(2);
    for attempt in 1..=MAX_RETRIES {
        match fetch_models_dev_once(upstream).await {
            Ok(bytes) => {
                if attempt > 1 {
                    tracing::info!(attempt, "models.dev fetch succeeded after retry");
                }
                return Ok(bytes);
            }
            Err(e) => {
                handle_fetch_attempt_error(attempt, MAX_RETRIES, &mut backoff, e).await?;
            }
        }
    }
    Err(CoreError::UpstreamConnection(
        "models.dev fetch: retry loop exhausted".into(),
    ))
}

fn map_fetch_upstream_error(
    e: openproxy_adapters::upstream::UpstreamError,
    ctx: &str,
) -> CoreError {
    if matches!(e, openproxy_adapters::upstream::UpstreamError::Cancel) {
        CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected)
    } else {
        CoreError::UpstreamConnection(format!("{ctx}: {e}"))
    }
}

/// Single attempt to fetch raw JSON bytes from models.dev.
async fn fetch_models_dev_once(upstream: &Arc<UpstreamClient>) -> Result<bytes::Bytes> {
    let req = UpstreamRequest::get(MODELS_DEV_URL);
    let cancel = CancellationToken::new();
    let response = upstream
        .call(req, TimeoutProfile::ModelDiscovery, cancel)
        .await
        .map_err(|e| map_fetch_upstream_error(e, "models.dev fetch"))?;

    let status = response.status;
    let body = response
        .collect()
        .await
        .map_err(|e| map_fetch_upstream_error(e, "models.dev body read"))?;

    if !status.is_success() {
        let text = String::from_utf8_lossy(&body);
        return Err(CoreError::UpstreamError {
            status: status.as_u16(),
            provider: "models.dev".into(),
            model: "<sync>".into(),
            body: text.to_string(),
            is_proxy_rotated: false,
            class: openproxy_types::UpstreamErrorClass::Generic,
            is_hard_skip: false,
        });
    }

    Ok(body)
}

/// Background sync task using a `ServiceContainer` for dependency injection.
pub async fn start_sync_scheduler_with_container(
    services: &crate::di::ServiceContainer,
    check_interval_secs: u64,
) -> Result<()> {
    let db_pool = services.db_pool()?;
    let upstream_client = services.upstream_client()?;
    start_sync_scheduler(db_pool, upstream_client, check_interval_secs).await;
    Ok(())
}

fn process_models_dev_sync_payload(db_pool: &openproxy_db::DbPool, body: &[u8]) -> usize {
    let count = {
        let conn = db_pool.writer();
        match upsert_models_dev(body, &conn) {
            Ok(n) => {
                tracing::info!("models.dev sync: {} rows upserted", n);
                n
            }
            Err(e) => {
                tracing::warn!(error = %e, "models.dev sync upsert failed");
                return 0;
            }
        }
    };

    if count > 0 {
        {
            let conn = db_pool.writer();
            match enrich_models_from_sync(&conn) {
                Ok(n) => tracing::info!("models.dev sync: enriched {} model rows", n),
                Err(e) => tracing::warn!(error = %e, "models.dev sync enrich failed"),
            }
        }
        {
            let conn = db_pool.writer();
            match auto_create_combos(&conn) {
                Ok(n) => {
                    if n > 0 {
                        tracing::info!("models.dev sync: created {} auto-combos", n);
                    }
                }
                Err(e) => tracing::warn!(error = %e, "models.dev sync auto-combo failed"),
            }
        }
    }
    count
}

async fn run_single_sync_iteration(
    db_pool: &Arc<openproxy_db::DbPool>,
    upstream_client: &Arc<UpstreamClient>,
) {
    tracing::info!("models.dev sync: starting");

    let body = match fetch_models_dev(upstream_client).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "models.dev sync fetch failed");
            return;
        }
    };

    let db_pool_clone = Arc::clone(db_pool);
    let count =
        tokio::task::spawn_blocking(move || process_models_dev_sync_payload(&db_pool_clone, &body))
            .await
            .unwrap_or(0);

    if count > 0 {
        tracing::info!("models.dev sync: complete");
    }
}

pub async fn start_sync_scheduler(
    db_pool: std::sync::Arc<openproxy_db::DbPool>,
    upstream_client: Arc<UpstreamClient>,
    check_interval_secs: u64,
) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(check_interval_secs));

    loop {
        tick.tick().await;
        run_single_sync_iteration(&db_pool, &upstream_client).await;
    }
}

/// One-shot sync + enrich + auto-combo, called from the admin handler.
pub async fn run_one_shot(
    db_pool: std::sync::Arc<openproxy_db::DbPool>,
    upstream_client: Arc<UpstreamClient>,
) -> Result<String> {
    let body = fetch_models_dev(&upstream_client).await?;

    let res = tokio::task::spawn_blocking(move || -> Result<(usize, usize, usize, usize)> {
        let count = {
            let conn = db_pool.writer();
            upsert_models_dev(&body, &conn)?
        };
        if count == 0 {
            return Ok((0, 0, 0, 0));
        }

        let enriched = {
            let conn = db_pool.writer();
            enrich_models_from_sync(&conn)?
        };

        let combos = {
            let conn = db_pool.writer();
            auto_create_combos(&conn)?
        };

        let repriced = {
            let conn = db_pool.writer();
            recompute_costs(&conn)?
        };

        Ok((count, enriched, combos, repriced))
    })
    .await
    .map_err(|e| openproxy_types::error::CoreError::Internal(e.to_string()))??;

    let (count, enriched, combos, repriced) = res;
    if count == 0 {
        return Ok("No new models.dev data".into());
    }

    Ok(format!(
        "Synced {count} models, enriched {enriched} model rows, created {combos} auto-combos, re-priced {repriced} usage rows"
    ))
}
