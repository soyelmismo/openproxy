//! Runner común y helpers de orquestación para rondas de refresco de contratos.
//!
//! Centraliza la frontera de adquisición cancelable vs. commit atómico (`publish`),
//! así como el patrón común de jobs basados en drivers de release (`ContractDriverJob`).

use std::future::Future;
use std::sync::Arc;

use openproxy_adapters::client_contract::{Candidate, ContractError, ContractStore};
use openproxy_adapters::upstream::CancellationToken;

use super::client_contract::{ContractRefreshJob, RoundOutcome, publish};

/// Orquesta una ronda de adquisición cancelable seguida de publicación atómica.
///
/// La adquisición observa `cancel` de forma `biased`; si se cancela antes de
/// completar la adquisición, devuelve `Ok(RoundOutcome::Cancelled)` sin tocar el store.
/// Si la adquisición produce `None`, devuelve `Ok(RoundOutcome::Unchanged)`.
/// Si produce `Some(candidate)`, cruza la frontera de commit y drena `publish`
/// hasta el final en `spawn_blocking` ignorando el token de cancelación.
pub async fn run_refresh_round<T, Fut>(
    store: &Arc<ContractStore<T>>,
    cancel: &CancellationToken,
    acquire: impl FnOnce() -> Fut,
) -> Result<RoundOutcome, ContractError>
where
    T: serde::Serialize + serde::de::DeserializeOwned + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<Option<Candidate<T>>, ContractError>>,
{
    let acquired = tokio::select! {
        biased;
        () = cancel.cancelled() => return Ok(RoundOutcome::Cancelled),
        acquired = acquire() => acquired?,
    };

    let Some(candidate) = acquired else {
        return Ok(RoundOutcome::Unchanged);
    };

    publish(store, candidate).await
}

/// Orquesta una ronda de captura o generación que produce un `Candidate<T>`
/// directo (o falla), con adquisición cancelable y publicación atómica.
pub async fn run_capture_round<T, Fut>(
    store: &Arc<ContractStore<T>>,
    cancel: &CancellationToken,
    capture: impl FnOnce() -> Fut,
) -> Result<RoundOutcome, ContractError>
where
    T: serde::Serialize + serde::de::DeserializeOwned + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<Candidate<T>, ContractError>>,
{
    let candidate = tokio::select! {
        biased;
        () = cancel.cancelled() => return Ok(RoundOutcome::Cancelled),
        acquired = capture() => acquired?,
    };

    publish(store, candidate).await
}

/// Helper para contratos cuyo driver expone `refresh(&self) -> Result<Option<Candidate<T>>, ContractError>`.
pub trait RefreshableDriver<T>: Send + Sync + 'static {
    fn refresh(
        &self,
    ) -> impl Future<Output = Result<Option<Candidate<T>>, ContractError>> + Send;
}

/// Trabajo genérico de refresco de contrato respaldado por un driver de release y un store tipado.
pub struct DriverContractJob<D, T> {
    name: &'static str,
    driver: Arc<D>,
    store: Arc<ContractStore<T>>,
}

impl<D, T> DriverContractJob<D, T> {
    #[must_use]
    pub fn new(name: &'static str, driver: Arc<D>, store: Arc<ContractStore<T>>) -> Self {
        Self { name, driver, store }
    }

    #[must_use]
    pub fn driver(&self) -> &Arc<D> {
        &self.driver
    }

    #[must_use]
    pub fn store(&self) -> &Arc<ContractStore<T>> {
        &self.store
    }
}

impl<D, T> ContractRefreshJob for DriverContractJob<D, T>
where
    D: RefreshableDriver<T>,
    T: serde::Serialize + serde::de::DeserializeOwned + Clone + Send + Sync + 'static,
{
    fn name(&self) -> &'static str {
        self.name
    }

    async fn refresh_round(
        &self,
        cancel: &CancellationToken,
    ) -> Result<RoundOutcome, ContractError> {
        let driver = Arc::clone(&self.driver);
        run_refresh_round(&self.store, cancel, || async move { driver.refresh().await }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_adapters::client_contract::{
        ClientIdentity, ContractId, ContractLimits, ContractValidator, Snapshot, StoreConfig,
    };
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Debug)]
    struct DummyProfile {
        tag: String,
    }

    struct DummyValidator;
    impl ContractValidator<DummyProfile> for DummyValidator {
        fn rule_set_id(&self) -> &'static str {
            "dummy"
        }
        fn extractor_version(&self) -> u32 {
            1
        }
        fn validate(
            &self,
            _candidate: &Candidate<DummyProfile>,
            _previous: Option<&Snapshot<DummyProfile>>,
            _limits: &ContractLimits,
        ) -> Result<(), ContractError> {
            Ok(())
        }
    }

    struct MockDriver {
        candidate: Option<Candidate<DummyProfile>>,
        hang: AtomicBool,
    }

    impl RefreshableDriver<DummyProfile> for MockDriver {
        async fn refresh(&self) -> Result<Option<Candidate<DummyProfile>>, ContractError> {
            if self.hang.load(Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
            Ok(self.candidate.clone())
        }
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("runner-{tag}-{}-{stamp}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn test_store(name: &str) -> (Arc<ContractStore<DummyProfile>>, std::path::PathBuf) {
        let dir = temp_dir(name);
        let store = Arc::new(ContractStore::new(
            ContractId::Codex,
            StoreConfig::new(&dir).with_limits(ContractLimits::tight()),
            None,
            Arc::new(DummyValidator),
        ));
        (store, dir)
    }

    fn sample_candidate() -> Candidate<DummyProfile> {
        Candidate::without_catalog(
            ContractId::Codex,
            ClientIdentity::unversioned("official"),
            "dummy",
            1,
            vec![openproxy_adapters::client_contract::SourceRef::pinned(
                "https://example.test/source",
                "1.0.0",
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                openproxy_adapters::client_contract::EvidenceKind::ReleaseMetadata,
            )],
            DummyProfile {
                tag: "v1".to_string(),
            },
        )
    }

    #[tokio::test]
    async fn runner_unchanged_when_driver_returns_none() {
        let (store, dir) = test_store("none");
        let driver = Arc::new(MockDriver {
            candidate: None,
            hang: AtomicBool::new(false),
        });
        let job = DriverContractJob::new("test_job", driver, store);
        let cancel = CancellationToken::new();
        let outcome = job.refresh_round(&cancel).await.expect("refresh_round");
        assert_eq!(outcome, RoundOutcome::Unchanged);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn runner_activated_when_driver_returns_candidate() {
        let (store, dir) = test_store("some");
        let driver = Arc::new(MockDriver {
            candidate: Some(sample_candidate()),
            hang: AtomicBool::new(false),
        });
        let job = DriverContractJob::new("test_job", driver, store);
        let cancel = CancellationToken::new();
        let outcome = job.refresh_round(&cancel).await.expect("refresh_round");
        assert!(matches!(outcome, RoundOutcome::Activated { .. }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn runner_cancelled_discards_before_publish() {
        let (store, dir) = test_store("cancelled");
        let driver = Arc::new(MockDriver {
            candidate: Some(sample_candidate()),
            hang: AtomicBool::new(true),
        });
        let job = DriverContractJob::new("test_job", driver, store);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let outcome = job.refresh_round(&cancel).await.expect("refresh_round");
        assert_eq!(outcome, RoundOutcome::Cancelled);
        let _ = std::fs::remove_dir_all(dir);
    }
}
