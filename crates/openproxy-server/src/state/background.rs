//! Background task wiring and supervisor task scheduling.

use crate::background::client_contract::codex_runtime_refresh_service;
use crate::background::{claude_refresh_service, codebuddy_refresh_service};
use openproxy_adapters::adapters;
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_core::discovery_scheduler::{self, DiscoveryScheduler};
use openproxy_db as db;
use openproxy_db::MasterKey;
use parking_lot::RwLock;
use std::sync::Arc;

pub(crate) struct SpawnBackgroundTasksArgs {
    pub(crate) db_pool: Arc<db::DbPool>,
    pub(crate) config: openproxy_core::AppConfig,
    pub(crate) recording_ttl_secs_cell: Arc<RwLock<i64>>,
    pub(crate) maintenance_cell: Arc<RwLock<openproxy_types::config::MaintenanceConfig>>,
    pub(crate) vacuum_status: Arc<RwLock<super::VacuumStatus>>,
    pub(crate) backfill_status: Arc<RwLock<super::BackfillStatus>>,
    pub(crate) master_key: Arc<MasterKey>,
    pub(crate) adapters: Arc<RwLock<Arc<Vec<adapters::ProviderAdapterEnum>>>>,
    pub(crate) upstream_client: Arc<UpstreamClient>,
    pub(crate) oauth_provider_registry: Arc<openproxy_core::oauth::OAuthProviderRegistry>,
}

/// Compuerta de auto-refresh de un contrato: activada por defecto, se
/// desactiva explícitamente con `0` o `false`.
fn contract_auto_refresh_enabled(env_key: &str) -> bool {
    !std::env::var(env_key).is_ok_and(|value| value == "0" || value == "false")
}

pub(crate) fn spawn_background_tasks(
    supervisor: &crate::background::BackgroundSupervisor,
    args: SpawnBackgroundTasksArgs,
) {
    let SpawnBackgroundTasksArgs {
        db_pool,
        config,
        recording_ttl_secs_cell,
        maintenance_cell,
        vacuum_status,
        backfill_status,
        master_key,
        adapters,
        upstream_client,
        oauth_provider_registry,
    } = args;

    // Refresco de los contratos de cliente (Claude Code, Codex, CodeBuddy):
    // un único ciclo compartido, un servicio por proveedor bajo el mismo
    // supervisor, con una compuerta de auto-refresh individual. En tests no
    // se arranca nada: nada de red ni capturas accidentales.
    if !cfg!(test) {
        // La generación activa de Codex (LKG en disco o semilla embebida)
        // queda cargada al arrancar, sin red: la primera petición no paga la
        // lectura y el refresher parte de un snapshot ya cargado.
        if let Err(error) =
            openproxy_adapters::client_contract::providers::codex_state::ensure_loaded()
        {
            tracing::warn!(
                %error,
                "no se pudo cargar el contrato Codex al arrancar; la primera petición lo reintentará"
            );
        }
        if contract_auto_refresh_enabled("OPENPROXY_CLAUDE_CODE_PROFILE_AUTO_REFRESH") {
            supervisor.spawn(claude_refresh_service());
        }
        if contract_auto_refresh_enabled("OPENPROXY_CODEX_CONTRACT_AUTO_REFRESH") {
            supervisor.spawn(codex_runtime_refresh_service(Arc::clone(&upstream_client)));
        }
        if contract_auto_refresh_enabled("OPENPROXY_CODEBUDDY_CONTRACT_AUTO_REFRESH") {
            supervisor.spawn(codebuddy_refresh_service(Arc::clone(&upstream_client)));
        }
    }

    // Boot backfill: provider seed + repricing + bootstrap key. First
    // tick fires immediately so the dashboard sees a "backfilling"
    // banner rather than waiting for the interval. We use a 6h cadence
    // on subsequent passes so pricing drift after a models.dev sync is
    // picked up.
    supervisor.spawn(crate::background::BackfillService {
        db_pool: Arc::clone(&db_pool),
        backfill_status: Arc::clone(&backfill_status),
        interval: std::time::Duration::from_hours(6),
    });

    supervisor.spawn(crate::background::CooldownPrunerService {
        db_pool: Arc::clone(&db_pool),
        interval: std::time::Duration::from_mins(1),
    });

    supervisor.spawn(crate::background::RecordingTtlPrunerService {
        db_pool: Arc::clone(&db_pool),
        recording_ttl_secs_cell,
        interval: std::time::Duration::from_mins(1),
    });

    supervisor.spawn(crate::background::OAuthRefreshService {
        db_pool: Arc::clone(&db_pool),
        master_key: Arc::clone(&master_key),
        upstream_client: Arc::clone(&upstream_client),
        oauth_provider_registry: Arc::clone(&oauth_provider_registry),
    });

    let models_dev_enabled =
        std::env::var("MODELS_DEV_SYNC_ENABLED").is_ok_and(|v| v == "1" || v == "true");
    if models_dev_enabled {
        let interval_secs: u64 = std::env::var("MODELS_DEV_SYNC_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(86_400);
        supervisor.spawn(crate::background::ModelsDevSyncService {
            db_pool: Arc::clone(&db_pool),
            upstream_client: Arc::clone(&upstream_client),
            interval_secs,
        });
    }

    supervisor.spawn(crate::background::QuotaSyncService {
        db_pool: Arc::clone(&db_pool),
        config,
        upstream_client: Arc::clone(&upstream_client),
        master_key: Arc::clone(&master_key),
        adapters: Arc::clone(&adapters),
        oauth_provider_registry: Arc::clone(&oauth_provider_registry),
    });

    supervisor.spawn(crate::background::MiniMaxCheckinService {
        db_pool: Arc::clone(&db_pool),
        upstream_client: Arc::clone(&upstream_client),
        master_key: Arc::clone(&master_key),
    });

    supervisor.spawn(crate::background::FreeProxiesSyncService {
        db_pool: Arc::clone(&db_pool),
    });

    supervisor.spawn(crate::background::FreeProxiesValidatorService {
        db_pool: Arc::clone(&db_pool),
    });

    supervisor.spawn(crate::background::MaintenanceVacuumService {
        db_pool,
        maintenance_cell,
        vacuum_status,
    });
}

pub(crate) fn start_discovery_scheduler(
    db_pool: Arc<openproxy_db::DbPool>,
    master_key: Arc<openproxy_db::secrets::MasterKey>,
    adapters: &Arc<RwLock<Arc<Vec<openproxy_adapters::adapters::ProviderAdapterEnum>>>>,
    upstream_client: Arc<openproxy_adapters::upstream::UpstreamClient>,
) -> DiscoveryScheduler {
    let adapters_clone = Arc::clone(&adapters.read());
    discovery_scheduler::start(
        db_pool,
        master_key,
        adapters_clone,
        upstream_client,
        openproxy_core::discovery_scheduler::DiscoverySchedulerConfig::default(),
    )
}

pub(crate) fn spawn_rate_limiter_cleanup(
    supervisor: &crate::background::BackgroundSupervisor,
    rate_limiter: Arc<dyn openproxy_core::rate_limit::RateLimiter>,
) {
    supervisor.spawn(crate::background::RateLimiterCleanupService {
        rate_limiter,
        interval: std::time::Duration::from_mins(5),
    });
}

pub(crate) fn spawn_memory_cleanup(
    supervisor: &crate::background::BackgroundSupervisor,
    db_pool: Arc<openproxy_db::DbPool>,
    selection_registry: Arc<openproxy_types::SelectionRegistry>,
    circuit_breaker: openproxy_pipeline::circuit_breaker::CircuitBreakerRegistry,
    predictive_limiter: Arc<openproxy_pipeline::PredictiveRateLimiter>,
    api_key_cache: Arc<
        dashmap::DashMap<String, (Arc<openproxy_core::api_keys::ApiKey>, std::time::Instant)>,
    >,
) {
    supervisor.spawn(crate::background::MemoryCleanupService {
        db_pool,
        selection_registry,
        circuit_breaker,
        predictive_limiter,
        api_key_cache,
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic, clippy::wildcard_imports)]

    use super::*;
    use crate::background::BackgroundSupervisor;
    use crate::background::client_contract::{
        ContractRefreshJob, ContractRefreshService, RefreshPolicy, RoundOutcome,
    };
    use openproxy_adapters::client_contract::ContractError;
    use openproxy_adapters::upstream::CancellationToken;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    /// Trabajo de fixture: cuenta su ronda sin tocar la red.
    struct FixtureJob {
        name: &'static str,
        rounds: Arc<AtomicU64>,
    }

    impl ContractRefreshJob for FixtureJob {
        fn name(&self) -> &'static str {
            self.name
        }

        async fn refresh_round(
            &self,
            _cancel: &CancellationToken,
        ) -> Result<RoundOutcome, ContractError> {
            self.rounds.fetch_add(1, Ordering::Relaxed);
            Ok(RoundOutcome::Unchanged)
        }
    }

    /// Espera acotada a que el contador alcance el objetivo; falla en vez de
    /// colgarse si la ronda nunca corre.
    async fn wait_for(counter: &AtomicU64, target: u64) {
        for _ in 0..500 {
            if counter.load(Ordering::Relaxed) >= target {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("la ronda de arranque nunca se ejecutó");
    }

    /// El registro de producción arranca un refresher de contrato por
    /// proveedor bajo el MISMO supervisor, cada uno con un único loop (sin
    /// duplicados), y el shutdown los drena a los tres. Jobs de fixture: cero
    /// llamadas al upstream.
    #[tokio::test]
    async fn the_three_contract_refreshers_start_under_one_supervisor_and_drain_on_shutdown() {
        let supervisor = BackgroundSupervisor::new();
        let policy = RefreshPolicy::default().with_interval(Duration::from_secs(3600));

        let mut services = Vec::new();
        let mut counters = Vec::new();
        for name in [
            "claude_code_profile",
            "codex_contract",
            "codebuddy_contract",
        ] {
            let rounds = Arc::new(AtomicU64::new(0));
            let service = Arc::new(ContractRefreshService::new(
                FixtureJob {
                    name,
                    rounds: Arc::clone(&rounds),
                },
                policy,
            ));
            assert!(
                service.spawn_under(&supervisor),
                "{name} debe arrancar bajo el supervisor"
            );
            services.push(service);
            counters.push(rounds);
        }

        // Una sola ronda de arranque por servicio: ningún loop duplicado.
        for counter in &counters {
            wait_for(counter, 1).await;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
        for counter in &counters {
            assert_eq!(
                counter.load(Ordering::Relaxed),
                1,
                "cada servicio ejecuta exactamente una ronda de arranque"
            );
        }

        // El shutdown drena los tres: ninguno queda en vuelo ni ronda de más.
        supervisor.shutdown_and_wait().await;
        for service in &services {
            assert!(
                !service.in_flight(),
                "la compuerta queda libre tras el drenaje"
            );
            assert_eq!(service.stats().rounds, 1);
            assert_eq!(service.stats().unchanged, 1);
            assert_eq!(service.stats().cancelled, 0);
            assert_eq!(service.stats().failures, 0);
        }
    }
}
