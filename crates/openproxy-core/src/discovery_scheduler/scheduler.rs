//! Scheduler loop lifecycle and background task spawning.

use super::config::{DiscoveryScheduler, DiscoverySchedulerConfig};
use super::runner::run_one_tick;
use crate::ids::ProviderId;
use crate::providers;
use crate::seed;
use openproxy_adapters::adapters::ProviderAdapterEnum;
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

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
    let mut task_count = 0usize;

    // Collect all provider candidates:
    // 1. Built-in providers defined in seed
    // 2. Custom providers existing in DB
    let mut seen_providers = std::collections::HashSet::new();
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

    for (provider, adapter) in resolved_providers {
        let pool = Arc::clone(&db_pool);
        let key = Arc::clone(&master_key);
        let upstream = Arc::clone(&upstream_client);
        let task_cancel = parent_cancel.child_token();
        let interval = config.interval_secs.max(1);
        let initial_stagger = config.initial_stagger_secs;

        let first_delay_secs = if initial_stagger == 0 {
            0
        } else {
            rand::random::<u64>() % (initial_stagger + 1)
        };

        tracing::info!(
            provider = %provider,
            interval_secs = interval,
            first_delay_secs,
            "discovery scheduler for {provider} starting",
        );

        tokio::spawn(run_one_provider(RunProviderParams {
            provider,
            adapter,
            db_pool: pool,
            master_key: key,
            upstream_client: upstream,
            interval_secs: interval,
            first_delay: Duration::from_secs(first_delay_secs),
            cancel: task_cancel,
        }));
        task_count += 1;
    }

    // Dynamic supervisor: checks periodically for newly created providers
    let supervisor_pool = Arc::clone(&db_pool);
    let supervisor_key = Arc::clone(&master_key);
    let supervisor_upstream = Arc::clone(&upstream_client);
    let supervisor_cancel = parent_cancel.child_token();
    let supervisor_interval = config.interval_secs.max(1);
    let supervisor_adapters = Arc::clone(&adapters);

    tokio::spawn(async move {
        let initial_wait = Duration::from_secs(supervisor_interval.min(60));
        tokio::select! {
            () = sleep(initial_wait) => {}
            () = supervisor_cancel.cancelled() => return,
        }

        loop {
            let db_providers = {
                let r = supervisor_pool.reader();
                providers::list(&r).unwrap_or_default()
            };

            for p in db_providers {
                if !p.active {
                    continue;
                }
                let adapter = if let Some(a) = supervisor_adapters.iter().find(|a| a.id() == &p.id)
                {
                    ProviderAdapterEnum::clone(a)
                } else if seed::is_builtin(p.id.as_str()) {
                    continue;
                } else {
                    ProviderAdapterEnum::Custom(Box::new(
                        openproxy_adapters::adapters::CustomAdapter::from_provider_row(&p),
                    ))
                };

                run_one_tick(
                    p.id,
                    adapter,
                    &supervisor_pool,
                    &supervisor_key,
                    &supervisor_upstream,
                )
                .await;

                tokio::select! {
                    () = sleep(Duration::from_millis(200)) => {}
                    () = supervisor_cancel.cancelled() => return,
                }
            }

            tokio::select! {
                () = sleep(Duration::from_secs(supervisor_interval)) => {}
                () = supervisor_cancel.cancelled() => return,
            }
        }
    });

    DiscoveryScheduler {
        cancel: parent_cancel,
        task_count,
    }
}

struct RunProviderParams {
    provider: ProviderId,
    adapter: openproxy_adapters::adapters::ProviderAdapterEnum,
    db_pool: Arc<DbPool>,
    master_key: Arc<MasterKey>,
    upstream_client: Arc<UpstreamClient>,
    interval_secs: u64,
    first_delay: Duration,
    cancel: CancellationToken,
}

async fn run_one_provider(params: RunProviderParams) {
    let RunProviderParams {
        provider,
        adapter,
        db_pool,
        master_key,
        upstream_client,
        interval_secs,
        first_delay,
        cancel,
    } = params;

    if !first_delay.is_zero() {
        tokio::select! {
            () = sleep(first_delay) => {}
            () = cancel.cancelled() => {
                tracing::info!(
                    provider = %provider,
                    "discovery scheduler for {provider} shutting down",
                );
                return;
            }
        }
    }

    loop {
        run_one_tick(
            ProviderId::new(provider.as_str()),
            ProviderAdapterEnum::clone(&adapter),
            &db_pool,
            &master_key,
            &upstream_client,
        )
        .await;

        tokio::select! {
            () = sleep(Duration::from_secs(interval_secs)) => {}
            () = cancel.cancelled() => {
                tracing::info!(
                    provider = %provider,
                    "discovery scheduler for {provider} shutting down",
                );
                return;
            }
        }
    }
}
