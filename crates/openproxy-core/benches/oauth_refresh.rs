use criterion::{Criterion, criterion_group, criterion_main};
use std::time::Duration;

fn bench_oauth_refresh(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("oauth_refresh");

    // Sequential simulation (Old behavior)
    group.bench_function("sequential_refresh_10", |b| {
        b.to_async(&rt).iter(|| async {
            for i in 0..10 {
                if i > 0 {
                    tokio::time::sleep(Duration::from_millis(3)).await; // scaled down STAGGER_DELAY
                }
                tokio::time::sleep(Duration::from_millis(5)).await; // simulated I/O latency
                tokio::time::sleep(Duration::from_millis(2)).await; // scaled down SETTLE_GAP
            }
        });
    });

    // Concurrent token bucket simulation (New behavior)
    group.bench_function("concurrent_refresh_10", |b| {
        b.to_async(&rt).iter(|| async {
            use governor::{Quota, RateLimiter};
            use std::num::NonZeroU32;
            use std::sync::Arc;

            let quota = Quota::with_period(Duration::from_millis(3))
                .unwrap()
                .allow_burst(NonZeroU32::new(1).unwrap());
            let limiter = Arc::new(RateLimiter::direct(quota));

            let mut join_set = tokio::task::JoinSet::new();
            for _ in 0..10 {
                let lim = Arc::clone(&limiter);
                join_set.spawn(async move {
                    lim.until_ready().await;
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    tokio::time::sleep(Duration::from_millis(2)).await;
                });
            }
            while join_set.join_next().await.is_some() {}
        });
    });
    group.finish();
}

criterion_group!(benches, bench_oauth_refresh);
criterion_main!(benches);
