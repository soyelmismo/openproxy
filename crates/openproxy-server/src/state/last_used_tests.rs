use super::last_used::LastUsedBatcher;
use openproxy_types::ids::ApiKeyId;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_timestamps_share_commits_and_are_persisted_before_reply() {
    let pool = Arc::new(openproxy_db::DbPool::test_pool_with_prefix("last-used-batch").unwrap());
    let key = {
        let w = pool.writer();
        openproxy_core::api_keys::create(
            &w,
            openproxy_core::api_keys::CreateApiKeyInput {
                scopes: vec!["chat".into()],
                ..Default::default()
            },
            "batch-test",
        )
        .unwrap()
        .0
        .id
    };
    let commits = Arc::new(AtomicUsize::new(0));
    {
        let counter = Arc::clone(&commits);
        pool.writer()
            .commit_hook(Some(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                false
            }))
            .unwrap();
    }
    let batcher = Arc::new(LastUsedBatcher::default());
    let barrier = Arc::new(tokio::sync::Barrier::new(100));
    let mut tasks = vec![];
    for _ in 0..100 {
        let b = Arc::clone(&batcher);
        let p = Arc::clone(&pool);
        let gate = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            gate.wait().await;
            b.touch(p, ApiKeyId(key.0)).await.unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert!(commits.load(Ordering::SeqCst) < 10);
    let row = openproxy_core::api_keys::get_by_id(&pool.reader(), key)
        .unwrap()
        .unwrap();
    assert!(row.last_used_at.is_some());
}
