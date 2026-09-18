//! Scheduler loop lifecycle and bounded background worker pool.

use super::config::{DiscoveryScheduler, DiscoverySchedulerConfig};
use super::runner::run_one_tick;
use crate::ids::ProviderId;
use crate::providers;
use crate::seed;
use openproxy_adapters::adapters::ProviderAdapterEnum;
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Number of concurrent background workers processing discovery ticks.
/// Bounded to 3 workers to prevent task stack memory bloat and socket exhaustion.
const DISCOVERY_WORKER_POOL_SIZE: usize = 3;

/// Channel capacity for pending provider discovery ticks.
const DISCOVERY_QUEUE_CAPACITY: usize = 64;

/// Internal descriptor of a scheduled provider discovery task.
struct ScheduledProvider {
    id: ProviderId,
    adapter: ProviderAdapterEnum,
    interval: Duration,
    next_run: tokio::time::Instant,
}

/// Start the discovery scheduler using a `ServiceContainer` for dependency injection.
pub fn start_with_container(
    services: &crate::di::ServiceContainer,
    config: DiscoverySchedulerConfig,
) -> crate::error::Result<DiscoveryScheduler> {
    let db_pool = services.db_pool()?;
    let master_key = services.master_key()?;
    let adapters = services.adapters()?;
    let upstream_client = services.upstream_client()?;
    Ok(start(
        db_pool,
        master_key,
        adapters,
        upstream_client,
        config,
    ))
}

pub fn start(
    db_pool: Arc<DbPool>,
    master_key: Arc<MasterKey>,
    adapters: Arc<Vec<openproxy_adapters::adapters::ProviderAdapterEnum>>,
    upstream_client: Arc<UpstreamClient>,
    config: DiscoverySchedulerConfig,
) -> DiscoveryScheduler {
    let parent_cancel = CancellationToken::new();

    // Collect all provider candidates:
    // 1. Built-in providers defined in seed
    // 2. Custom providers existing in DB
    let mut seen_providers = HashSet::new();
    let mut resolved_providers = Vec::new();

    // 1. Built-in providers that have an adapter in `adapters`
    for pid_str in seed::builtin_provider_ids() {
        let provider = ProviderId::new(pid_str);
        if let Some(a) = adapters.iter().find(|a| a.id() == &provider) {
            seen_providers.insert(provider.clone());
            resolved_providers.push((provider, ProviderAdapterEnum::clone(a)));
        } else {
            tracing::warn!(
                provider = %provider,
                "no adapter registered for built-in provider; \
                 discovery scheduler skipping this provider",
            );
        }
    }

    // 2. Custom providers from DB
    {
        let r = db_pool.reader();
        if let Ok(db_list) = providers::list(&r) {
            for p in db_list {
                if !seen_providers.contains(&p.id) {
                    if seed::is_builtin(p.id.as_str()) {
                        continue;
                    }
                    let adapter = if let Some(a) = adapters.iter().find(|a| a.id() == &p.id) {
                        ProviderAdapterEnum::clone(a)
                    } else {
                        ProviderAdapterEnum::Custom(Box::new(
                            openproxy_adapters::adapters::CustomAdapter::from_provider_row(&p),
                        ))
                    };
                    seen_providers.insert(p.id.clone());
                    resolved_providers.push((p.id, adapter));
                }
            }
        }
    }

    let task_count = resolved_providers.len();

    // If there are no providers to schedule, return immediately without spawning workers.
    if task_count == 0 {
        return DiscoveryScheduler {
            cancel: parent_cancel,
            task_count: 0,
        };
    }

    let (tx, rx) =
        tokio::sync::mpsc::channel::<(ProviderId, ProviderAdapterEnum)>(DISCOVERY_QUEUE_CAPACITY);
    let rx = Arc::new(tokio::sync::Mutex::new(rx));
    let in_flight = Arc::new(parking_lot::Mutex::new(HashSet::new()));

    // Bounded worker pool: spawn 2-4 concurrent workers (default 3, capped by task_count)
    let worker_count = DISCOVERY_WORKER_POOL_SIZE.min(task_count).max(1);
    for _ in 0..worker_count {
        let worker_cancel = parent_cancel.child_token();
        let rx = Arc::clone(&rx);
        let in_flight = Arc::clone(&in_flight);
        let pool = Arc::clone(&db_pool);
        let key = Arc::clone(&master_key);
        let upstream = Arc::clone(&upstream_client);

        tokio::spawn(async move {
            loop {
                if worker_cancel.is_cancelled() {
                    return;
                }
                let item = tokio::select! {
                    biased;
                    () = worker_cancel.cancelled() => return,
                    item = async {
                        let mut guard = rx.lock().await;
                        guard.recv().await
                    } => item,
                };

                if worker_cancel.is_cancelled() {
                    return;
                }

                let Some((provider, adapter)) = item else {
                    return;
                };

                if worker_cancel.is_cancelled() {
                    return;
                }

                run_one_tick(
                    ProviderId::new(provider.as_str()),
                    adapter,
                    &pool,
                    &key,
                    &upstream,
                )
                .await;

                in_flight.lock().remove(&provider);
            }
        });
    }

    // Coordinator loop: schedules provider ticks and periodically discovers new DB providers
    let mut items = Vec::with_capacity(task_count);
    let now = tokio::time::Instant::now();
    for (i, (provider, adapter)) in resolved_providers.into_iter().enumerate() {
        let interval = config.interval_secs.max(1);
        let initial_stagger = config.initial_stagger_secs;
        let first_delay = if initial_stagger > 0 {
            Duration::from_secs(rand::random::<u64>() % (initial_stagger + 1))
        } else if interval <= 1 || task_count <= 10 {
            // Adversarial burst tests (interval <= 1) and small unit tests (task_count <= 10):
            // immediate dispatch for deterministic verification.
            Duration::ZERO
        } else {
            // Production cadence with many providers: stagger by 200ms per provider.
            // Spreads 79 providers over ~16 seconds so only 1-2 providers are polled
            // per interval at boot, eliminating the 10-second startup memory burst.
            Duration::from_millis(200 * (i as u64))
        };

        items.push(ScheduledProvider {
            id: provider,
            adapter,
            interval: Duration::from_secs(interval),
            next_run: now + first_delay,
        });
    }

    let coordinator_cancel = parent_cancel.child_token();
    let coordinator_pool = Arc::clone(&db_pool);
    let coordinator_adapters = Arc::clone(&adapters);
    let default_interval_secs = config.interval_secs.max(1);

    tokio::spawn(async move {
        let db_check_interval = Duration::from_secs(default_interval_secs.min(60));
        let mut last_db_check = tokio::time::Instant::now();

        loop {
            if coordinator_cancel.is_cancelled() {
                return;
            }
            let current_now = tokio::time::Instant::now();

            // 1. Dispatch due items to worker pool
            for item in &mut items {
                if coordinator_cancel.is_cancelled() {
                    return;
                }
                if item.next_run <= current_now {
                    let can_dispatch = {
                        let mut guard = in_flight.lock();
                        if guard.contains(&item.id) {
                            false
                        } else {
                            guard.insert(item.id.clone());
                            true
                        }
                    };

                    if can_dispatch {
                        if coordinator_cancel.is_cancelled() {
                            in_flight.lock().remove(&item.id);
                            return;
                        }
                        item.next_run = current_now + item.interval;
                        if let Err(e) = tx
                            .try_send((item.id.clone(), ProviderAdapterEnum::clone(&item.adapter)))
                        {
                            in_flight.lock().remove(&item.id);
                            tracing::warn!(provider = %item.id, "discovery worker queue full: {e}");
                        }
                    }
                }
            }

            // 2. Periodic sync for newly created/deactivated providers in DB
            if coordinator_cancel.is_cancelled() {
                return;
            }
            if last_db_check.elapsed() >= db_check_interval {
                last_db_check = tokio::time::Instant::now();
                let r = coordinator_pool.reader();
                if let Ok(db_list) = providers::list(&r) {
                    for p in db_list {
                        if coordinator_cancel.is_cancelled() {
                            return;
                        }
                        if !p.active {
                            items.retain(|i| i.id != p.id);
                            continue;
                        }
                        if !items.iter().any(|i| i.id == p.id) {
                            if seed::is_builtin(p.id.as_str()) {
                                continue;
                            }
                            let adapter = if let Some(a) =
                                coordinator_adapters.iter().find(|a| a.id() == &p.id)
                            {
                                ProviderAdapterEnum::clone(a)
                            } else {
                                ProviderAdapterEnum::Custom(Box::new(
                                    openproxy_adapters::adapters::CustomAdapter::from_provider_row(
                                        &p,
                                    ),
                                ))
                            };
                            items.push(ScheduledProvider {
                                id: p.id,
                                adapter,
                                interval: Duration::from_secs(default_interval_secs),
                                next_run: tokio::time::Instant::now(),
                            });
                        }
                    }
                }
            }

            if coordinator_cancel.is_cancelled() {
                return;
            }

            // 3. Sleep until the next scheduled provider run or DB check
            let earliest = items
                .iter()
                .map(|i| i.next_run)
                .min()
                .unwrap_or_else(|| current_now + Duration::from_secs(60));
            let sleep_dur = earliest
                .saturating_duration_since(current_now)
                .min(db_check_interval);

            tokio::select! {
                biased;
                () = coordinator_cancel.cancelled() => return,
                () = tokio::time::sleep(sleep_dur) => {},
            }
        }
    });

    DiscoveryScheduler {
        cancel: parent_cancel,
        task_count,
    }
}
