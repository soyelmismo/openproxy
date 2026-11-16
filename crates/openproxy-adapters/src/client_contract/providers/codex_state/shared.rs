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

/// Verifica que el candidato declare un `catalog_ref` y, si incluye bytes en
/// memoria, que sean UTF-8 válidos.
pub(crate) fn validate_catalog_utf8<'a, T>(
    candidate: &'a Candidate<T>,
    missing_msg: &str,
) -> Result<Option<&'a str>, ContractError> {
    if candidate.catalog_ref.is_none() {
        return Err(ContractError::Rejected(missing_msg.to_string()));
    }
    match candidate.catalog_body() {
        Some(bytes) => {
            let text = std::str::from_utf8(bytes).map_err(|error| {
                ContractError::Rejected(format!("el catálogo no es utf-8: {error}"))
            })?;
            Ok(Some(text))
        }
        None => Ok(None),
    }
}

/// Genera los puntos de entrada globales (`default_store_root`, `RUNTIME`,
/// `runtime`, `ensure_loaded`, `current_snapshot`, `activated_version`,
/// `refresh_validated_with_transport` y `refresh_validated`) para un proveedor
/// de contrato.
macro_rules! define_global_provider_runtime {
    ($runtime_ty:ident, $provider_ty:ty, $profile_ty:ty, $env_var:literal) => {
        pub type $runtime_ty =
            $crate::client_contract::providers::codex_state::shared::ProviderRuntime<$provider_ty>;

        fn default_store_root() -> std::path::PathBuf {
            $crate::client_contract::providers::codex_state::shared::default_store_root_for(
                $env_var,
            )
        }

        static RUNTIME: std::sync::LazyLock<$runtime_ty> =
            std::sync::LazyLock::new(|| $runtime_ty::new(default_store_root()));

        #[must_use]
        pub fn runtime() -> &'static $runtime_ty {
            &RUNTIME
        }

        pub fn ensure_loaded() -> Result<
            std::sync::Arc<$crate::client_contract::Snapshot<$profile_ty>>,
            $crate::client_contract::ContractError,
        > {
            if let Some(snapshot) = RUNTIME.snapshot() {
                return Ok(snapshot);
            }
            RUNTIME.load()
        }

        #[must_use]
        pub fn current_snapshot()
        -> Option<std::sync::Arc<$crate::client_contract::Snapshot<$profile_ty>>> {
            RUNTIME.snapshot()
        }

        #[must_use]
        pub fn activated_version() -> Option<String> {
            RUNTIME
                .snapshot()
                .map(|snapshot| snapshot.payload.version.clone())
        }

        pub async fn refresh_validated_with_transport(
            transport: std::sync::Arc<dyn $crate::client_contract::ContractTransport>,
        ) -> Option<String> {
            let _ = ensure_loaded();
            RUNTIME.refresh_validated(transport).await
        }

        #[cfg(feature = "upstream-hyper")]
        pub async fn refresh_validated(
            upstream_client: &std::sync::Arc<$crate::upstream::UpstreamClient>,
        ) -> Option<String> {
            use $crate::client_contract::UpstreamHttpTransport;

            let _ = ensure_loaded();
            let transport =
                std::sync::Arc::new(UpstreamHttpTransport::new(std::sync::Arc::clone(
                    upstream_client,
                )));
            refresh_validated_with_transport(transport).await
        }

        #[cfg(not(feature = "upstream-hyper"))]
        pub use $crate::client_contract::providers::codex_state::shared::refresh_validated_no_upstream as refresh_validated;
    };
}
pub(crate) use define_global_provider_runtime;

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
    #[must_use]
    pub fn store_handle(&self) -> Arc<ContractStore<P::Profile>> {
        self.inner.store_handle()
    }

    /// Driver de una ronda de refresh sobre `transport`, con la cache y los
    /// límites de este runtime.
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
    pub fn load(&self) -> Result<Arc<Snapshot<P::Profile>>, ContractError> {
        self.inner.load()
    }

    /// El snapshot ya cargado, si el runtime ya sirvió uno.
    #[must_use]
    pub fn snapshot(&self) -> Option<Arc<Snapshot<P::Profile>>> {
        self.inner.snapshot()
    }

    /// Activa un candidato verificado (anti-downgrade + persistencia).
    pub fn commit(
        &self,
        candidate: &Candidate<P::Profile>,
    ) -> Result<Option<Arc<Snapshot<P::Profile>>>, ContractError> {
        self.inner.commit(candidate)
    }

    /// Resuelve el pin contra `transport` y activa el candidato si verifica
    /// las reglas compiladas; si no, la generación activa no cambia.
    pub async fn refresh(
        &self,
        transport: Arc<dyn ContractTransport>,
    ) -> Result<RefreshReport<P::Profile>, ContractError> {
        self.inner.refresh_with(transport, P::refresh).await
    }

    /// Igual que [`Self::refresh`], pero devuelve la versión recién activada
    /// **solo** cuando el perfil activado tiene identidad activable.
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

#[cfg(test)]
pub(crate) mod test_support {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::client_contract::{
        ContractError, ContractLimits, ContractTransport, FetchRequest, FetchResponse,
    };

    /// Transporte en memoria que responde desde un mapa de URL y registra las
    /// peticiones observadas.
    #[derive(Default)]
    pub(crate) struct MapTransport {
        pub(crate) responses: Mutex<HashMap<String, FetchResponse>>,
        seen: Mutex<Vec<FetchRequest>>,
    }

    impl MapTransport {
        pub(crate) fn with(entries: Vec<(String, FetchResponse)>) -> Self {
            Self {
                responses: Mutex::new(entries.into_iter().collect()),
                seen: Mutex::new(Vec::new()),
            }
        }

        pub(crate) fn set(&self, url: String, response: FetchResponse) {
            self.responses.lock().unwrap().insert(url, response);
        }

        pub(crate) fn requests(&self) -> Vec<FetchRequest> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl ContractTransport for MapTransport {
        fn fetch(
            &self,
            request: &FetchRequest,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<FetchResponse, ContractError>> + Send + '_>,
        > {
            self.seen.lock().unwrap().push(request.clone());
            let response = self
                .responses
                .lock()
                .unwrap()
                .get(&request.url)
                .cloned()
                .unwrap_or(FetchResponse {
                    status: 404,
                    headers: Vec::new(),
                    body: Vec::new(),
                });
            Box::pin(async move { Ok(response) })
        }
    }

    pub(crate) fn temp_dir(prefix: &str, name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("{prefix}-{name}-{stamp}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub(crate) fn fixture_bytes(relative: &str) -> Vec<u8> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
        std::fs::read(root.join(relative))
            .unwrap_or_else(|error| panic!("fixture {relative}: {error}"))
    }

    pub(crate) fn limits() -> ContractLimits {
        ContractLimits {
            catalog_bytes: 8 * 1024 * 1024,
            source_bytes: 8 * 1024 * 1024,
            ..ContractLimits::default()
        }
    }

    pub(crate) fn ok_raw(bytes: &[u8]) -> FetchResponse {
        FetchResponse {
            status: 200,
            headers: Vec::new(),
            body: bytes.to_vec(),
        }
    }

    pub(crate) fn ok_json(body: &str) -> FetchResponse {
        ok_raw(body.as_bytes())
    }

    pub(crate) fn ok_etag_raw(body: &[u8]) -> FetchResponse {
        FetchResponse {
            status: 200,
            headers: vec![("etag".to_string(), "\"v1\"".to_string())],
            body: body.to_vec(),
        }
    }

    pub(crate) fn ok_etag_json(body: &str) -> FetchResponse {
        ok_etag_raw(body.as_bytes())
    }
}
