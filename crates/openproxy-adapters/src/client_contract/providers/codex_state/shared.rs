//! Piezas compartidas por los runtimes de contrato del paquete.
//!
//! `codebuddy_state` los reutiliza a través de `super::codex_state`: la raíz
//! por defecto del LKG en disco, el chequeo de las reglas compiladas que todo
//! candidato debe declarar y el facade [`ProviderRuntime`], parametrizado por
//! [`ContractProvider`] con las piezas concretas de cada proveedor.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use crate::client_contract::{
    Candidate, ContractError, ContractId, ContractLimits, ContractStore, ContractTransport,
    ContractValidator, GenericContractRuntime, RefreshReport, Snapshot, TransportCache,
};

/// Reglas compiladas que todo candidato debe declarar: el mismo `rule_set_id`
/// y la misma `extractor_version` que el runtime compilado.
pub(crate) fn check_compiled_rules<T>(
    candidate: &Candidate<T>,
    rule_set_id: &'static str,
    extractor_version: u32,
) -> Result<(), ContractError> {
    if candidate.rule_set_id != rule_set_id {
        return Err(ContractError::Rejected(format!(
            "el candidato declara el rule set {:?} y el runtime compilado es {:?}",
            candidate.rule_set_id, rule_set_id
        )));
    }
    if candidate.extractor_version != extractor_version {
        return Err(ContractError::Rejected(format!(
            "el candidato usa el extractor v{} y el runtime compilado es v{}",
            candidate.extractor_version, extractor_version
        )));
    }
    Ok(())
}

/// Raíz por defecto del LKG en disco: `env_var` si está definida y no vacía,
/// si no `~/.openproxy/contracts`.
pub(crate) fn default_store_root_for(env_var: &str) -> PathBuf {
    if let Ok(dir) = std::env::var(env_var)
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".openproxy").join("contracts")
}

/// Sin transporte upstream embebido no hay refresh de red: el runtime global
/// sigue sirviendo la generación activa.
///
/// Reexportado como `refresh_validated` por `codex_state` y `codebuddy_state`
/// cuando el crate no trae la feature `upstream-hyper`.
#[cfg(not(feature = "upstream-hyper"))]
#[must_use]
pub async fn refresh_validated_no_upstream(
    _upstream_client: &Arc<crate::upstream::UpstreamClient>,
) -> Option<String> {
    None
}

/// Piezas concretas que un proveedor de contrato aporta al facade
/// [`ProviderRuntime`]: el perfil del snapshot, el driver tipado de la ronda,
/// el contrato gobernado, la semilla embebida, el validador compilado y la
/// ronda de refresh.
pub trait ContractProvider: 'static + Sized {
    /// Perfil de dominio del snapshot.
    type Profile: serde::Serialize + serde::de::DeserializeOwned + Clone + Send + Sync + 'static;
    /// Driver tipado de una ronda de refresh.
    type Driver: Send + Sync + 'static;
    /// El contrato que gobierna el runtime.
    const CONTRACT_ID: ContractId;
    /// La semilla embebida del pin.
    fn seed() -> Snapshot<Self::Profile>;
    /// El validador de las reglas compiladas.
    fn validator() -> Arc<dyn ContractValidator<Self::Profile>>;
    /// El driver de una ronda sobre la infraestructura compartida.
    fn driver(
        transport: Arc<dyn ContractTransport>,
        cache: Arc<TransportCache>,
        limits: ContractLimits,
    ) -> Arc<Self::Driver>;
    /// La ronda de refresh del driver: resuelve el pin y devuelve el
    /// candidato a commitear (`None` si la ronda no produjo uno).
    fn refresh(
        driver: Arc<Self::Driver>,
    ) -> impl Future<Output = Result<Option<Candidate<Self::Profile>>, ContractError>> + Send;
}

/// Capacidad fail-closed de un perfil de contrato: la versión anunciada solo
/// se eleva cuando la identidad activada es usable.
///
/// La implementa el perfil del proveedor cuyo pin no puede verificar la
/// correspondencia completa (véase `codebuddy_state`): los demás perfiles no
/// necesitan esta puerta y `refresh_validated` no existe para ellos.
pub trait ActivatableVersion {
    /// La versión del perfil si su identidad es activable; `None` en caso
    /// contrario.
    fn activatable_version(&self) -> Option<String>;
}

/// Runtime de contrato parametrizado por [`ContractProvider`]: delega en
/// [`GenericContractRuntime`].
pub struct ProviderRuntime<P: ContractProvider> {
    inner: GenericContractRuntime<P::Profile, P::Driver>,
}

impl<P: ContractProvider> ProviderRuntime<P> {
    /// Crea el runtime con límites por defecto y el LKG en `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_limits(root, ContractLimits::default())
    }

    /// Crea el runtime con límites explícitos y el LKG en `root`.
    #[must_use]
    pub fn with_limits(root: impl Into<PathBuf>, limits: ContractLimits) -> Self {
        Self {
            inner: GenericContractRuntime::new(
                P::CONTRACT_ID,
                root,
                limits,
                P::seed(),
                P::validator(),
                P::driver,
            ),
        }
    }

    /// El store tipado, para commits directos de un candidato verificado.
    #[must_use]
    pub fn store(&self) -> &ContractStore<P::Profile> {
        self.inner.store()
    }

    /// El store tipado como handle compartido: **el mismo** store que este
    /// runtime lee.
    ///
    /// El server construye con él el job de refresh, de modo que una
    /// generación activada por el refresher es visible de inmediato para el
    /// hot path (`pin_request_contract` → `ensure_loaded`). Nunca hay un
    /// segundo store desconectado del que lee la petición.
    #[must_use]
    pub fn store_handle(&self) -> Arc<ContractStore<P::Profile>> {
        self.inner.store_handle()
    }

    /// Driver de una ronda de refresh sobre `transport`, con la cache y los
    /// límites de este runtime.
    ///
    /// La cache es la del runtime: el refresher no arranca con una cache fría
    /// paralela, sino con los mismos condicionales que ya se usaron.
    #[must_use]
    pub fn release_driver(&self, transport: Arc<dyn ContractTransport>) -> Arc<P::Driver> {
        self.inner.release_driver(transport)
    }

    /// La cache de transporte compartida con el driver.
    #[must_use]
    pub fn cache(&self) -> &Arc<TransportCache> {
        self.inner.cache()
    }

    /// Snapshot activo: el LKG en disco si existe, si no la semilla embebida.
    ///
    /// # Errors
    /// Errores del store (LKG corrupto o semilla inválida).
    pub fn load(&self) -> Result<Arc<Snapshot<P::Profile>>, ContractError> {
        self.inner.load()
    }

    /// El snapshot ya cargado, si el runtime ya sirvió uno.
    #[must_use]
    pub fn snapshot(&self) -> Option<Arc<Snapshot<P::Profile>>> {
        self.inner.snapshot()
    }

    /// Activa un candidato verificado (anti-downgrade + persistencia).
    ///
    /// # Errors
    /// Errores del store o del validador.
    pub fn commit(
        &self,
        candidate: &Candidate<P::Profile>,
    ) -> Result<Option<Arc<Snapshot<P::Profile>>>, ContractError> {
        self.inner.commit(candidate)
    }

    /// Resuelve el pin contra `transport` y activa el candidato si verifica
    /// las reglas compiladas; si no, la generación activa no cambia.
    ///
    /// # Errors
    /// Errores del driver o del store.
    pub async fn refresh(
        &self,
        transport: Arc<dyn ContractTransport>,
    ) -> Result<RefreshReport<P::Profile>, ContractError> {
        self.inner.refresh_with(transport, P::refresh).await
    }

    /// Igual que [`Self::refresh`], pero devuelve la versión recién activada
    /// **solo** cuando el perfil activado tiene identidad activable.
    ///
    /// Es el contrato fail-closed del refresh: un candidato verificado puede
    /// publicar el catálogo como overlay, pero la versión anunciada **no** se
    /// eleva mientras la correspondencia paquete→commit no esté verificada.
    /// `None` no es un error: es el resultado correcto.
    pub async fn refresh_validated(&self, transport: Arc<dyn ContractTransport>) -> Option<String>
    where
        P::Profile: ActivatableVersion,
    {
        match self.refresh(transport).await {
            Ok(RefreshReport::Activated { snapshot, .. }) => snapshot.payload.activatable_version(),
            Ok(RefreshReport::Unchanged { .. } | RefreshReport::Rejected { .. }) => None,
            Err(error) => {
                tracing::warn!(
                    contract = %P::CONTRACT_ID,
                    error = %error,
                    "refresco del contrato rechazado; se conserva la generación activa"
                );
                None
            }
        }
    }
}
