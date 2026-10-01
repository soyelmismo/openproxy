use crate::models_dev_sync::run_sync_scheduler;
use openproxy_adapters::upstream::{CancellationToken, UpstreamClient};
use openproxy_db::DbPool;
use std::sync::Arc;

#[tokio::test]
async fn run_sync_scheduler_pre_cancelled_returns_no_http() {
    let pool = DbPool::test_pool_with_prefix("openproxy-sync-client-test").unwrap();
    let upstream = UpstreamClient::new();
    let cancel = CancellationToken::new();
    cancel.cancel();

    // With pre-cancelled token, run_sync_scheduler must return immediately without HTTP calls
    run_sync_scheduler(Arc::new(pool), upstream, 3_600, cancel).await;
}

#[tokio::test]
async fn run_sync_scheduler_interval_positive_clamped_to_at_least_one() {
    let pool = DbPool::test_pool_with_prefix("openproxy-sync-interval-test").unwrap();
    let upstream = UpstreamClient::new();
    let cancel = CancellationToken::new();
    cancel.cancel();

    // Passing 0 interval verifies interval.max(1) guarantees a positive period, preventing Tokio panic
    run_sync_scheduler(Arc::new(pool), upstream, 0, cancel).await;
}
