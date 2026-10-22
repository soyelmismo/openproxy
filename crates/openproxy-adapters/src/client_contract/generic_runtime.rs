//! Runtime genérico de contrato de cliente: store tipado, cache de transporte y ciclo de refresh.
//!
//! Abstrae el patrón común implementado por [`super::providers::codex_state::CodexContractRuntime`]
//! y [`super::providers::codebuddy_state::CodeBuddyContractRuntime`], unificando la inicialización
//! de [`ContractStore`], la gestión de la [`TransportCache`], los límites, commits, snapshots
//! y el orquestador de refresh.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use crate::client_contract::{
    Candidate, ContractError, ContractId, ContractLimits, ContractStore, ContractTransport,
    ContractValidator, RefreshReport, Snapshot, StoreConfig, TransportCache,
};

/// Trait para instanciar el driver de release/refresh asociado a un runtime de contrato.
pub trait ReleaseDriverFactory<T>: Send + Sync + 'static {
    /// Tipo concreto del driver producido.
    type Driver: Send + Sync + 'static;

    /// Construye una nueva instancia del driver compartiendo la cache y los límites del runtime.
    fn make_driver(
        &self,
        transport: Arc<dyn ContractTransport>,
        cache: Arc<TransportCache>,
        limits: ContractLimits,
    ) -> Arc<Self::Driver>;
}

/// Implementación por función o closure para [`ReleaseDriverFactory`].
impl<T, D, F> ReleaseDriverFactory<T> for F
where
    D: Send + Sync + 'static,
    F: Fn(Arc<dyn ContractTransport>, Arc<TransportCache>, ContractLimits) -> Arc<D>
        + Send
        + Sync
        + 'static,
{
    type Driver = D;

    fn make_driver(
        &self,
        transport: Arc<dyn ContractTransport>,
        cache: Arc<TransportCache>,
        limits: ContractLimits,
    ) -> Arc<Self::Driver> {
        (self)(transport, cache, limits)
    }
}

/// Runtime genérico que encapsula [`ContractStore`], [`TransportCache`], límites y semilla embebida.
///
/// Parámetros de tipo:
/// - `P`: El tipo de payload/perfil del contrato (e.g. `CodexProfile`, `CodeBuddyProfile`).
/// - `D`: El tipo concreto del driver de refresh (e.g. `CodexReleaseDriver`, `CodeBuddyReleaseDriver`).
pub struct GenericContractRuntime<P, D> {
    store: Arc<ContractStore<P>>,
    cache: Arc<TransportCache>,
    limits: ContractLimits,
    seed_generation: String,
    driver_builder: Arc<dyn ReleaseDriverFactory<P, Driver = D>>,
}

impl<P, D> GenericContractRuntime<P, D>
where
    P: serde::Serialize + serde::de::DeserializeOwned + Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    /// Construye el runtime genérico especificando la semilla embebida, el validador y la fábrica de drivers.
    pub fn new<F>(
        contract_id: ContractId,
        root: impl Into<PathBuf>,
        limits: ContractLimits,
        seed: Snapshot<P>,
        validator: Arc<dyn ContractValidator<P>>,
        driver_builder: F,
    ) -> Self
    where
        F: ReleaseDriverFactory<P, Driver = D>,
    {
        let seed_generation = seed.generation.clone();
        Self {
            store: Arc::new(ContractStore::new(
                contract_id,
                StoreConfig::new(root).with_limits(limits),
                Some(seed),
                validator,
            )),
            cache: Arc::new(TransportCache::new()),
            limits,
            seed_generation,
            driver_builder: Arc::new(driver_builder),
        }
    }

    /// El store tipado, para commits directos de un candidato verificado.
    #[must_use]
    pub fn store(&self) -> &ContractStore<P> {
        self.store.as_ref()
    }

    /// El store tipado como handle compartido: **el mismo** store que este runtime lee.
    #[must_use]
    pub fn store_handle(&self) -> Arc<ContractStore<P>> {
        Arc::clone(&self.store)
    }

    /// Driver de una ronda de refresh sobre `transport`, con la cache y los límites de este runtime.
    #[must_use]
    pub fn release_driver(&self, transport: Arc<dyn ContractTransport>) -> Arc<D> {
        self.driver_builder
            .make_driver(transport, Arc::clone(&self.cache), self.limits)
    }

    /// La cache de transporte compartida con el driver.
    #[must_use]
    pub fn cache(&self) -> &Arc<TransportCache> {
        &self.cache
    }

    /// Límites configurados en el runtime.
    #[must_use]
    pub fn limits(&self) -> ContractLimits {
        self.limits
    }

    /// Snapshot activo: el LKG en disco si existe, si no la semilla embebida.
    ///
    /// # Errors
    /// Errores del store (LKG corrupto o semilla inválida).
    pub fn load(&self) -> Result<Arc<Snapshot<P>>, ContractError> {
        self.store.load()
    }

    /// El snapshot ya cargado, si el runtime ya sirvió uno.
    #[must_use]
    pub fn snapshot(&self) -> Option<Arc<Snapshot<P>>> {
        self.store.snapshot()
    }

    /// Activa un candidato verificado (anti-downgrade + persistencia).
    ///
    /// # Errors
    /// Errores del store o del validador.
    pub fn commit(
        &self,
        candidate: &Candidate<P>,
    ) -> Result<Option<Arc<Snapshot<P>>>, ContractError> {
        self.store.commit(candidate)
    }

    /// La generación activa (la semilla si aún no se cargó nada).
    #[must_use]
    pub fn active_generation(&self) -> String {
        self.store.snapshot().map_or_else(
            || self.seed_generation.clone(),
            |snapshot| snapshot.generation.clone(),
        )
    }

    /// Orquesta una ronda de refresh genérica ejecutando la acción de refresh del driver
    /// y commiteando el candidato resultante al store si existe.
    ///
    /// # Errors
    /// Errores producidos por el driver o por el commit en el store.
    pub async fn refresh_with<Fut>(
        &self,
        transport: Arc<dyn ContractTransport>,
        refresh_action: impl FnOnce(Arc<D>) -> Fut,
    ) -> Result<RefreshReport<P>, ContractError>
    where
        Fut: Future<Output = Result<Option<Candidate<P>>, ContractError>>,
    {
        let driver = self.release_driver(transport);
        match refresh_action(driver).await? {
            Some(candidate) => match self.store.commit(&candidate)? {
                Some(snapshot) => Ok(RefreshReport::Activated {
                    generation: snapshot.generation.clone(),
                    snapshot,
                }),
                None => Ok(RefreshReport::Unchanged {
                    generation: self.active_generation(),
                }),
            },
            None => Ok(RefreshReport::Unchanged {
                generation: self.active_generation(),
            }),
        }
    }
}
