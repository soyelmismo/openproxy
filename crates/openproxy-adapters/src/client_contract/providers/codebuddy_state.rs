//! Runtime de contrato CodeBuddy: store global, seed embebido y refresh validado.
//!
//! Une el driver del paquete fijado ([`super::codebuddy_runtime`]) con el store
//! del base ([`crate::client_contract::store`]):
//!
//! * **Store global tipado.** [`runtime()`] expone un único
//!   [`CodeBuddyContractRuntime`] con el LKG en disco
//!   (`$OPENPROXY_CODEBUDDY_STORE_DIR` o `~/.openproxy/contracts`), accesible
//!   desde el server sin reconstruir nada por petición.
//! * **Bootstrap embebido.** [`embedded_snapshot`] construye un snapshot válido
//!   del pin actual (`2.164.0`) con los fixtures reales compilados en el
//!   binario ([`include_str!`]). Con disco vacío, [`CodeBuddyContractRuntime::load`]
//!   sirve el seed: el proxy arranca siempre con una generación activa.
//! * **Refresh validado.** [`CodeBuddyContractRuntime::refresh`] usa el driver y
//!   el validador [`CodeBuddyContractValidator`]; un candidato que no verifique
//!   los guards se rechaza y el snapshot activo no cambia.
//!
//! El runtime no duplica lógica del base: reutiliza `ContractStore` (LKG,
//! anti-downgrade, persistencia atómica), `TransportCache` (condicionales) y
//! `CodeBuddyReleaseDriver` (adquisición del pin). Solo añade el pegamento
//! específico de CodeBuddy: procedencia real, validación de reglas compiladas y
//! el punto de entrada global para el server.
//!
//! ## Gate de identidad, fail-closed
//!
//! El paquete npm de CodeBuddy no expone `gitHead`, así que
//! [`super::codebuddy::CodeBuddyContract::identity_activatable`] es `false`. El
//! validador **recomputa** ese gate desde las fuentes del candidato y exige que
//! la identidad no declare versión mientras la correspondencia no esté
//! verificada: ni la metadata ni el paquete por sí solos elevan la identidad
//! anunciada, y una versión distinta de la fijada se rechaza porque no hay
//! guards compilados para esa revisión. El catálogo sí se publica como overlay
//! separado, verificado por digest.

use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

#[cfg(test)]
use super::codebuddy::{
    CODEBUDDY_DEPLOYMENT_TYPE, CODEBUDDY_ENDPOINT, CODEBUDDY_PLATFORM, CODEBUDDY_STAGING_ENDPOINT,
};
use super::codebuddy::{CODEBUDDY_PACKAGE, CODEBUDDY_VERSION, CodeBuddyCatalog, CodeBuddyContract};
use super::codebuddy_runtime::{
    CODEBUDDY_REGISTRY_LATEST_URL, CODEBUDDY_RUNTIME_EXTRACTOR_VERSION,
    CODEBUDDY_RUNTIME_RULE_SET_ID, CodeBuddyProfile, CodeBuddyReleaseDriver,
    correspondence_from_evidence, parse_package_manifest, parse_product_document,
    pinned_package_url, pinned_product_url,
};
use crate::client_contract::{
    Candidate, ClientIdentity, ContractError, ContractId, ContractLimits, ContractStore,
    ContractTransport, ContractValidator, EvidenceKind, GenericContractRuntime,
    IDENTITY_ORIGIN_OFFICIAL, RefreshReport, Snapshot, SourceRef, TransportCache, VersionChannel,
    hex_digest, parse_version,
};

/// `package.json` del pin, compilado en el binario.
const PINNED_PACKAGE_JSON: &str =
    include_str!("../../../../../fixtures/codebuddy-extracted/package.json");
/// `product.json` del pin: catálogo y campos publicados.
const PINNED_PRODUCT_JSON: &str =
    include_str!("../../../../../fixtures/codebuddy-extracted/product.json");
/// Metadata mutable del registro, capturada para el pin.
const REGISTRY_LATEST_JSON: &str =
    include_str!("../../../../../fixtures/codebuddy/registry-latest.json");

/// Semilla embebida: un snapshot válido del pin actual construido desde los
/// fixtures reales, con procedencia por cada byte. Con disco vacío es la
/// primera generación activa.
fn embedded_seed() -> Snapshot<CodeBuddyProfile> {
    let manifest = parse_package_manifest(PINNED_PACKAGE_JSON.as_bytes())
        .unwrap_or_else(|error| panic!("el package.json embebido del pin es inválido: {error}"));
    assert!(
        manifest.name == CODEBUDDY_PACKAGE && manifest.version == CODEBUDDY_VERSION,
        "el package.json embebido declara {} {} y el pin es {CODEBUDDY_PACKAGE} {CODEBUDDY_VERSION}",
        manifest.name,
        manifest.version
    );
    let catalog = CodeBuddyCatalog::parse(PINNED_PRODUCT_JSON)
        .unwrap_or_else(|error| panic!("el catálogo embebido del pin es inválido: {error}"));
    let document = parse_product_document(PINNED_PRODUCT_JSON.as_bytes())
        .unwrap_or_else(|error| panic!("el product.json embebido del pin es inválido: {error}"));

    let sources = vec![
        SourceRef::pinned(
            CODEBUDDY_REGISTRY_LATEST_URL,
            manifest.version.clone(),
            hex_digest(REGISTRY_LATEST_JSON.as_bytes()),
            EvidenceKind::ReleaseMetadata,
        ),
        SourceRef::pinned(
            pinned_package_url(&manifest.version),
            manifest.version.clone(),
            hex_digest(PINNED_PACKAGE_JSON.as_bytes()),
            EvidenceKind::PackageManifest,
        ),
        SourceRef::pinned(
            pinned_product_url(&manifest.version),
            manifest.version.clone(),
            hex_digest(PINNED_PRODUCT_JSON.as_bytes()),
            EvidenceKind::RuntimeCatalog,
        ),
    ];

    let correspondence = correspondence_from_evidence(&sources);
    let identity_activatable = CodeBuddyContract {
        catalog: CodeBuddyCatalog::parse(PINNED_PRODUCT_JSON)
            .unwrap_or_else(|error| panic!("catálogo embebido inválido: {error}")),
        correspondence: correspondence.clone(),
    }
    .identity_activatable();
    let correspondence_reason = match correspondence {
        super::PackageCommitCorrespondence::Unverified { reason } => reason,
        super::PackageCommitCorrespondence::Verified { .. } => String::new(),
    };

    let profile = CodeBuddyProfile {
        package: manifest.name,
        version: manifest.version.clone(),
        endpoint: document.endpoint,
        staging_endpoint: document.staging_endpoint,
        deployment_type: document.deployment_type,
        platform: document.platform,
        official_endpoints: document.official_endpoints,
        catalog_models: catalog.models.len(),
        identity_activatable,
        correspondence_reason,
    };
    let identity = if identity_activatable {
        ClientIdentity::versioned(manifest.version, IDENTITY_ORIGIN_OFFICIAL)
    } else {
        ClientIdentity::unversioned(IDENTITY_ORIGIN_OFFICIAL)
    };

    let candidate = Candidate::without_catalog(
        ContractId::CodeBuddy,
        identity,
        CODEBUDDY_RUNTIME_RULE_SET_ID,
        CODEBUDDY_RUNTIME_EXTRACTOR_VERSION,
        sources,
        profile,
    )
    .with_catalog(PINNED_PRODUCT_JSON.as_bytes().to_vec(), "json");

    Snapshot::from_candidate(candidate)
        .unwrap_or_else(|error| panic!("la semilla embebida es inválida: {error}"))
}

/// Semilla embebida del pin: los fixtures reales compilados en el binario.
static SEED: LazyLock<Snapshot<CodeBuddyProfile>> = LazyLock::new(embedded_seed);

/// El snapshot embebido del pin, clonado.
///
/// Lo usan el runtime (como semilla del store) y los tests que necesitan un
/// snapshot válido sin tocar disco ni red.
#[must_use]
pub fn embedded_snapshot() -> Snapshot<CodeBuddyProfile> {
    (*SEED).clone()
}

/// Validador del contrato CodeBuddy.
///
/// Fail-closed sobre las reglas compiladas: la versión debe ser semver estable
/// **y la del pin compilado**, el paquete el del pin, el catálogo presente y
/// parseable, y la identidad sin versión mientras la correspondencia
/// paquete→commit no esté verificada. Cualquier hueco es `Err`: nunca se activa
/// por omisión.
pub struct CodeBuddyContractValidator;

impl ContractValidator<CodeBuddyProfile> for CodeBuddyContractValidator {
    fn rule_set_id(&self) -> &'static str {
        CODEBUDDY_RUNTIME_RULE_SET_ID
    }

    fn extractor_version(&self) -> u32 {
        CODEBUDDY_RUNTIME_EXTRACTOR_VERSION
    }

    fn catalog_capacity(&self, candidate: &Candidate<CodeBuddyProfile>) -> Option<usize> {
        Some(candidate.payload.catalog_models)
    }

    #[allow(clippy::too_many_lines)]
    fn validate(
        &self,
        candidate: &Candidate<CodeBuddyProfile>,
        _previous: Option<&Snapshot<CodeBuddyProfile>>,
        _limits: &ContractLimits,
    ) -> Result<(), ContractError> {
        if candidate.rule_set_id != CODEBUDDY_RUNTIME_RULE_SET_ID {
            return Err(ContractError::Rejected(format!(
                "el candidato declara el rule set {:?} y el runtime compilado es {:?}",
                candidate.rule_set_id, CODEBUDDY_RUNTIME_RULE_SET_ID
            )));
        }
        if candidate.extractor_version != CODEBUDDY_RUNTIME_EXTRACTOR_VERSION {
            return Err(ContractError::Rejected(format!(
                "el candidato usa el extractor v{} y el runtime compilado es v{}",
                candidate.extractor_version, CODEBUDDY_RUNTIME_EXTRACTOR_VERSION
            )));
        }

        let version = parse_version(&candidate.payload.version)?;
        if version.channel != VersionChannel::Stable {
            return Err(ContractError::Rejected(format!(
                "la versión {:?} no es del canal estable",
                candidate.payload.version
            )));
        }
        // Sin guards compilados para otra revisión, la versión del pin es la
        // única activable. Es la compuerta que impide elevar la identidad con
        // metadata o con un paquete descargado de otra revisión.
        if candidate.payload.version != CODEBUDDY_VERSION {
            return Err(ContractError::Rejected(format!(
                "el candidato anuncia la versión {:?} y el runtime compilado fija {CODEBUDDY_VERSION}: \
                 sin guards compilados para esa revisión el candidato se rechaza",
                candidate.payload.version
            )));
        }
        if candidate.payload.package != CODEBUDDY_PACKAGE {
            return Err(ContractError::Rejected(format!(
                "el candidato declara el paquete {:?} y el pin es {CODEBUDDY_PACKAGE:?}",
                candidate.payload.package
            )));
        }

        // Procedencia obligatoria: el paquete fijado y el producto del que sale
        // el catálogo, ambos con revisión = versión del pin.
        for (evidence, label) in [
            (EvidenceKind::PackageManifest, "package.json"),
            (EvidenceKind::RuntimeCatalog, "product.json"),
        ] {
            let supported = candidate.sources.iter().any(|source| {
                source.evidence == evidence
                    && source.revision.as_deref() == Some(candidate.payload.version.as_str())
            });
            if !supported {
                return Err(ContractError::Rejected(format!(
                    "el pin no prueba {label} para la revisión {}",
                    candidate.payload.version
                )));
            }
        }

        // Gate de identidad recomputado desde la evidencia, nunca desde el
        // payload: sin correspondencia verificada no hay identidad versionada.
        let correspondence = correspondence_from_evidence(&candidate.sources);
        if !correspondence.is_verified() {
            if candidate.payload.identity_activatable {
                return Err(ContractError::Rejected(
                    "el payload declara identidad activable sin correspondencia paquete→commit verificada"
                        .to_string(),
                ));
            }
            if candidate.client_identity.version.is_some() {
                return Err(ContractError::Rejected(
                    "la identidad anuncia una versión sin correspondencia paquete→commit verificada"
                        .to_string(),
                ));
            }
        }

        match candidate.catalog_ref.as_ref() {
            Some(_reference) => match candidate.catalog_body() {
                Some(bytes) => {
                    let text = std::str::from_utf8(bytes).map_err(|error| {
                        ContractError::Rejected(format!("el catálogo no es utf-8: {error}"))
                    })?;
                    let catalog = CodeBuddyCatalog::parse(text).map_err(|error| {
                        ContractError::Rejected(format!("el catálogo es inválido: {error}"))
                    })?;
                    if catalog.models.is_empty() {
                        return Err(ContractError::Rejected(
                            "el catálogo del pin está vacío".to_string(),
                        ));
                    }
                    if catalog.models.len() != candidate.payload.catalog_models {
                        return Err(ContractError::Rejected(format!(
                            "el catálogo declara {} modelos y el perfil {}",
                            catalog.models.len(),
                            candidate.payload.catalog_models
                        )));
                    }
                }
                None => {
                    // Referencia sin bytes: legal si ya están persistidos, los
                    // verifica el store por digest.
                }
            },
            None => {
                return Err(ContractError::Rejected(
                    "el perfil CodeBuddy requiere catálogo para activarse".to_string(),
                ));
            }
        }
        Ok(())
    }
}

/// Runtime del contrato CodeBuddy: delega en [`GenericContractRuntime`].
pub struct CodeBuddyContractRuntime {
    inner: GenericContractRuntime<CodeBuddyProfile, CodeBuddyReleaseDriver>,
}

impl CodeBuddyContractRuntime {
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
                ContractId::CodeBuddy,
                root,
                limits,
                embedded_snapshot(),
                Arc::new(CodeBuddyContractValidator),
                |transport, cache, limits| {
                    Arc::new(CodeBuddyReleaseDriver::new(transport, cache, limits))
                },
            ),
        }
    }

    /// El store tipado, para commits directos de un candidato verificado.
    #[must_use]
    pub fn store(&self) -> &ContractStore<CodeBuddyProfile> {
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
    pub fn store_handle(&self) -> Arc<ContractStore<CodeBuddyProfile>> {
        self.inner.store_handle()
    }

    /// Driver de una ronda de refresh sobre `transport`, con la cache y los
    /// límites de este runtime.
    #[must_use]
    pub fn release_driver(
        &self,
        transport: Arc<dyn ContractTransport>,
    ) -> Arc<CodeBuddyReleaseDriver> {
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
    pub fn load(&self) -> Result<Arc<Snapshot<CodeBuddyProfile>>, ContractError> {
        self.inner.load()
    }

    /// El snapshot ya cargado, si el runtime ya sirvió uno.
    #[must_use]
    pub fn snapshot(&self) -> Option<Arc<Snapshot<CodeBuddyProfile>>> {
        self.inner.snapshot()
    }

    /// Activa un candidato verificado (anti-downgrade + persistencia).
    ///
    /// # Errors
    /// Errores del store o del validador.
    pub fn commit(
        &self,
        candidate: &Candidate<CodeBuddyProfile>,
    ) -> Result<Option<Arc<Snapshot<CodeBuddyProfile>>>, ContractError> {
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
    ) -> Result<RefreshReport<CodeBuddyProfile>, ContractError> {
        self.inner
            .refresh_with(transport, |driver| async move { driver.refresh().await })
            .await
    }

    /// Igual que [`Self::refresh`], pero devuelve la versión recién activada
    /// **solo** cuando el perfil activado tiene identidad activable.
    ///
    /// Es el contrato fail-closed del refresh: un candidato verificado puede
    /// publicar el catálogo como overlay, pero la versión anunciada **no** se
    /// eleva mientras la correspondencia paquete→commit no esté verificada.
    /// `None` no es un error: es el resultado correcto.
    pub async fn refresh_validated(&self, transport: Arc<dyn ContractTransport>) -> Option<String> {
        match self.refresh(transport).await {
            Ok(RefreshReport::Activated { snapshot, .. })
                if snapshot.payload.identity_activatable =>
            {
                Some(snapshot.payload.version.clone())
            }
            Ok(
                RefreshReport::Activated { .. }
                | RefreshReport::Unchanged { .. }
                | RefreshReport::Rejected { .. },
            ) => None,
            Err(error) => {
                tracing::warn!(
                    contract = "codebuddy",
                    error = %error,
                    "refresco del contrato CodeBuddy rechazado; se conserva la generación activa"
                );
                None
            }
        }
    }
}

/// Raíz por defecto del LKG en disco.
fn default_store_root() -> PathBuf {
    if let Ok(dir) = std::env::var("OPENPROXY_CODEBUDDY_STORE_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".openproxy").join("contracts")
}

/// Runtime global del contrato CodeBuddy.
static RUNTIME: LazyLock<CodeBuddyContractRuntime> =
    LazyLock::new(|| CodeBuddyContractRuntime::new(default_store_root()));

/// El runtime global, accesible desde el server sin reconstruir nada.
#[must_use]
pub fn runtime() -> &'static CodeBuddyContractRuntime {
    &RUNTIME
}

/// Carga (o sirve) el snapshot activo del runtime global: LKG en disco o
/// semilla embebida del pin.
///
/// # Errors
/// Errores del store (LKG corrupto o semilla inválida).
pub fn ensure_loaded() -> Result<Arc<Snapshot<CodeBuddyProfile>>, ContractError> {
    if let Some(snapshot) = RUNTIME.snapshot() {
        return Ok(snapshot);
    }
    RUNTIME.load()
}

/// El snapshot activo del runtime global, si ya está cargado.
#[must_use]
pub fn current_snapshot() -> Option<Arc<Snapshot<CodeBuddyProfile>>> {
    RUNTIME.snapshot()
}

/// La versión del paquete del snapshot activo, si ya está cargado.
#[must_use]
pub fn activated_version() -> Option<String> {
    RUNTIME
        .snapshot()
        .map(|snapshot| snapshot.payload.version.clone())
}

/// Refresca el contrato contra el registro/unpkg con un transporte inyectado y
/// devuelve la versión recién activada (`None` si no hubo activación nueva, o
/// el candidato no verificó las reglas: la generación activa permanece).
///
/// Es la misma ruta que usa el server: el transporte solo cambia quién entrega
/// los bytes, nunca las reglas de validación.
pub async fn refresh_validated_with_transport(
    transport: std::sync::Arc<dyn ContractTransport>,
) -> Option<String> {
    let _ = ensure_loaded();
    RUNTIME.refresh_validated(transport).await
}

/// Refresca el contrato contra el registro/unpkg con el transporte del upstream
/// y devuelve la versión recién activada (`None` si no hubo activación nueva, o
/// el candidato no verificó las reglas: la generación activa permanece).
///
/// La versión solo se devuelve cuando el perfil activado es
/// `identity_activatable`: sin correspondencia paquete→commit verificada el
/// catálogo se publica como overlay, pero la identidad anunciada no se eleva.
#[cfg(feature = "upstream-hyper")]
pub async fn refresh_validated(
    upstream_client: &Arc<crate::upstream::UpstreamClient>,
) -> Option<String> {
    use crate::client_contract::UpstreamHttpTransport;

    let _ = ensure_loaded();
    let transport = Arc::new(UpstreamHttpTransport::new(Arc::clone(upstream_client)));
    refresh_validated_with_transport(transport).await
}

/// Sin transporte upstream embebido no hay refresh de red: el runtime global
/// sigue sirviendo la generación activa.
#[cfg(not(feature = "upstream-hyper"))]
#[must_use]
pub async fn refresh_validated(
    _upstream_client: &Arc<crate::upstream::UpstreamClient>,
) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::client_contract::providers::codebuddy_runtime::{
        CODEBUDDY_REGISTRY_LATEST_URL, CODEBUDDY_UNPKG_PREFIX, CodeBuddyReleaseDriver,
        pinned_package_url, pinned_product_url,
    };
    use crate::client_contract::{FetchRequest, FetchResponse, validate_candidate_shape};
    use std::collections::HashMap;
    use std::fs;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Transporte que responde desde un mapa de URL. Ninguna petición sale del
    /// proceso; una URL ausente se comporta como `404`.
    #[derive(Default)]
    struct MapTransport {
        responses: Mutex<HashMap<String, FetchResponse>>,
    }

    impl MapTransport {
        fn with(entries: Vec<(String, FetchResponse)>) -> Self {
            let mut responses = HashMap::new();
            for (url, response) in entries {
                responses.insert(url, response);
            }
            Self {
                responses: Mutex::new(responses),
            }
        }

        fn set(&self, url: String, response: FetchResponse) {
            self.responses.lock().unwrap().insert(url, response);
        }
    }

    impl ContractTransport for MapTransport {
        fn fetch(
            &self,
            request: &FetchRequest,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<FetchResponse, ContractError>> + Send + '_>,
        > {
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

    fn temp_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("openproxy-codebuddy-state-{name}-{stamp}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ok_json(body: &[u8]) -> FetchResponse {
        FetchResponse {
            status: 200,
            headers: Vec::new(),
            body: body.to_vec(),
        }
    }

    /// Mapa de red del pin completo, parametrizado por revisión: la misma forma
    /// que sirven el registro y unpkg para el pin.
    fn pin_map(version: &str, overrides: Vec<(&str, FetchResponse)>) -> MapTransport {
        let registry = if version == CODEBUDDY_VERSION {
            REGISTRY_LATEST_JSON.as_bytes().to_vec()
        } else {
            format!(
                r#"{{"name":"{CODEBUDDY_PACKAGE}","version":"{version}","dist":{{"integrity":"sha512-x"}}}}"#
            )
            .into_bytes()
        };
        let package = if version == CODEBUDDY_VERSION {
            PINNED_PACKAGE_JSON.as_bytes().to_vec()
        } else {
            format!(r#"{{"name":"{CODEBUDDY_PACKAGE}","version":"{version}"}}"#).into_bytes()
        };
        let map = MapTransport::with(vec![
            (
                CODEBUDDY_REGISTRY_LATEST_URL.to_string(),
                ok_json(&registry),
            ),
            (pinned_package_url(version), ok_json(&package)),
            (
                pinned_product_url(version),
                ok_json(PINNED_PRODUCT_JSON.as_bytes()),
            ),
        ]);
        for (relative, response) in overrides {
            let url = match relative {
                "product.json" => pinned_product_url(version),
                "package.json" => pinned_package_url(version),
                _ => format!("{CODEBUDDY_UNPKG_PREFIX}@{version}/{relative}"),
            };
            map.set(url, response);
        }
        map
    }

    /// El seed embebido es un snapshot válido del pin: procedencia por byte,
    /// catálogo incluido e identidad sin versión (correspondencia no
    /// verificada).
    #[test]
    fn the_embedded_seed_is_a_valid_pinned_snapshot() {
        let seed = embedded_snapshot();
        assert_eq!(seed.contract_id, ContractId::CodeBuddy);
        assert_eq!(seed.payload.package, CODEBUDDY_PACKAGE);
        assert_eq!(seed.payload.version, CODEBUDDY_VERSION);
        assert_eq!(seed.payload.endpoint, CODEBUDDY_ENDPOINT);
        assert_eq!(seed.payload.staging_endpoint, CODEBUDDY_STAGING_ENDPOINT);
        assert_eq!(seed.payload.deployment_type, CODEBUDDY_DEPLOYMENT_TYPE);
        assert_eq!(seed.payload.platform, CODEBUDDY_PLATFORM);
        assert_eq!(seed.payload.catalog_models, 22);
        assert!(!seed.payload.identity_activatable);
        assert!(seed.client_identity.version.is_none());
        assert_eq!(seed.sources.len(), 3, "metadata + paquete + producto");
        assert!(seed.catalog_body().is_some());
        validate_candidate_shape(&seed.as_candidate(), &ContractLimits::default()).unwrap();
    }

    /// Con disco vacío el store sirve la semilla: el proxy arranca con una
    /// generación activa sin tocar la red.
    #[test]
    fn an_empty_store_loads_the_embedded_seed() {
        let dir = temp_dir("seed");
        let runtime = CodeBuddyContractRuntime::new(&dir);
        let snapshot = runtime.load().unwrap();
        assert_eq!(snapshot.payload.version, CODEBUDDY_VERSION);
        assert_eq!(snapshot.generation, embedded_snapshot().generation);
        let _ = fs::remove_dir_all(&dir);
    }

    /// El validador acepta la semilla: es la referencia de "esto sí pasa".
    #[test]
    fn the_validator_accepts_the_embedded_seed() {
        let candidate = embedded_snapshot().as_candidate();
        CodeBuddyContractValidator
            .validate(&candidate, None, &ContractLimits::default())
            .unwrap();
        assert_eq!(
            CodeBuddyContractValidator.rule_set_id(),
            CODEBUDDY_RUNTIME_RULE_SET_ID
        );
        assert_eq!(
            CodeBuddyContractValidator.extractor_version(),
            CODEBUDDY_RUNTIME_EXTRACTOR_VERSION
        );
    }

    /// Una revisión nueva se rechaza: no hay guards compilados para ella, así
    /// que la identidad no se eleva ni con el paquete descargado.
    #[test]
    fn a_candidate_for_another_revision_is_rejected() {
        let dir = temp_dir("validator-version");
        let runtime = CodeBuddyContractRuntime::new(&dir);
        let seed = runtime.load().unwrap();

        let mut candidate = seed.as_candidate();
        candidate.payload.version = "2.165.0".to_string();
        let error = runtime.commit(&candidate).unwrap_err();
        assert!(
            matches!(error, ContractError::Rejected(ref message) if message.contains("guards compilados")),
            "got {error:?}"
        );
        assert_eq!(
            runtime.snapshot().unwrap().payload.version,
            CODEBUDDY_VERSION,
            "la generación activa no cambia"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Una identidad versionada sin correspondencia verificada se rechaza,
    /// aunque el payload diga que es activable.
    #[test]
    fn a_versioned_identity_without_verified_correspondence_is_rejected() {
        let dir = temp_dir("validator-identity");
        let runtime = CodeBuddyContractRuntime::new(&dir);
        let seed = runtime.load().unwrap();

        let mut candidate = seed.as_candidate();
        candidate.client_identity =
            ClientIdentity::versioned(CODEBUDDY_VERSION, IDENTITY_ORIGIN_OFFICIAL);
        let error = runtime.commit(&candidate).unwrap_err();
        assert!(
            matches!(error, ContractError::Rejected(ref message) if message.contains("correspondencia")),
            "got {error:?}"
        );

        let mut candidate = seed.as_candidate();
        candidate.payload.identity_activatable = true;
        let error = runtime.commit(&candidate).unwrap_err();
        assert!(
            matches!(error, ContractError::Rejected(ref message) if message.contains("sin correspondencia")),
            "got {error:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Un candidato sin catálogo no se activa.
    #[test]
    fn a_candidate_without_a_catalog_is_rejected() {
        let dir = temp_dir("validator-catalog");
        let runtime = CodeBuddyContractRuntime::new(&dir);
        let seed = runtime.load().unwrap();

        let mut candidate = seed.as_candidate();
        candidate.catalog_ref = None;
        candidate.catalog_bytes = None;
        let error = runtime.commit(&candidate).unwrap_err();
        assert!(
            matches!(error, ContractError::Rejected(ref message) if message.contains("catálogo")),
            "got {error:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Un pin sin procedencia del paquete fijado se rechaza.
    #[test]
    fn a_pin_without_package_provenance_is_rejected() {
        let dir = temp_dir("validator-provenance");
        let runtime = CodeBuddyContractRuntime::new(&dir);
        let seed = runtime.load().unwrap();

        let mut candidate = seed.as_candidate();
        candidate
            .sources
            .retain(|source| source.evidence != EvidenceKind::PackageManifest);
        let error = runtime.commit(&candidate).unwrap_err();
        assert!(
            matches!(error, ContractError::Rejected(ref message) if message.contains("package.json")),
            "got {error:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// LKG: un candidato activado sobrevive a un reinicio del runtime, con la
    /// misma generación content-addressed.
    #[test]
    fn an_activated_candidate_survives_a_restart() {
        let dir = temp_dir("lkg");
        let runtime = CodeBuddyContractRuntime::new(&dir);
        let seed = runtime.load().unwrap();

        // El catálogo cambia sin cambiar la revisión: es el overlay permitido
        // (mismo paquete, mismos guards, otro `product.json` verificado).
        let product = PINNED_PRODUCT_JSON.replace("\"Auto\"", "\"Auto (overlay)\"");
        let mut candidate = seed.as_candidate();
        candidate = candidate.with_catalog(product.as_bytes().to_vec(), "json");
        let published = runtime.commit(&candidate).unwrap().expect("activación");
        assert_ne!(published.generation, seed.generation);

        let restarted = CodeBuddyContractRuntime::new(&dir).load().unwrap();
        assert_eq!(restarted.generation, published.generation);
        assert_eq!(restarted.payload.version, CODEBUDDY_VERSION);
        let _ = fs::remove_dir_all(&dir);
    }

    /// El refresh validado activa el overlay del catálogo del pin y persiste el
    /// LKG; sin cambio de bytes, la ronda es `Unchanged`.
    #[tokio::test]
    async fn a_pinned_round_activates_the_catalog_overlay() {
        let dir = temp_dir("refresh-activate");
        let runtime = CodeBuddyContractRuntime::new(&dir);
        runtime.load().unwrap();

        let transport = Arc::new(pin_map(CODEBUDDY_VERSION, Vec::new()));
        let report = runtime.refresh(transport).await.unwrap();
        match &report {
            RefreshReport::Activated { snapshot, .. } => {
                assert_eq!(snapshot.payload.version, CODEBUDDY_VERSION);
                assert_eq!(snapshot.payload.catalog_models, 22);
            }
            other => panic!("el overlay debió activarse, got {other:?}"),
        }
        let restarted = CodeBuddyContractRuntime::new(&dir).load().unwrap();
        assert_eq!(restarted.payload.catalog_models, 22);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Una ronda cuya metadata anuncia otra versión conserva la generación
    /// activa: la metadata sola no eleva la identidad.
    #[tokio::test]
    async fn a_metadata_only_elevation_keeps_the_active_snapshot() {
        let dir = temp_dir("refresh-metadata");
        let runtime = CodeBuddyContractRuntime::new(&dir);
        let seed = runtime.load().unwrap();

        let transport = Arc::new(pin_map("2.165.0", Vec::new()));
        let error = runtime.refresh(transport).await.unwrap_err();
        assert!(
            matches!(error, ContractError::Rejected(ref message) if message.contains("metadata sola")),
            "got {error:?}"
        );
        assert_eq!(runtime.snapshot().unwrap().generation, seed.generation);
        let _ = fs::remove_dir_all(&dir);
    }

    /// El seam del refresh nunca eleva la versión anunciada mientras la
    /// correspondencia paquete→commit no esté verificada: el overlay del
    /// catálogo puede activarse, pero `refresh_validated` devuelve `None`.
    #[tokio::test]
    async fn the_validated_seam_never_elevates_while_correspondence_is_unverified() {
        let dir = temp_dir("refresh-seam");
        let runtime = CodeBuddyContractRuntime::new(&dir);
        runtime.load().unwrap();

        let transport = Arc::new(pin_map(CODEBUDDY_VERSION, Vec::new()));
        let elevated = runtime.refresh_validated(transport).await;
        assert_eq!(elevated, None, "sin gitHead no hay identidad activable");
        let snapshot = runtime.snapshot().unwrap();
        assert_eq!(snapshot.payload.version, CODEBUDDY_VERSION);
        assert!(!snapshot.payload.identity_activatable);
        let _ = fs::remove_dir_all(&dir);
    }

    /// El handle del store es el mismo store que el runtime lee: un commit
    /// hecho a través del handle aparece de inmediato en el snapshot activo.
    #[test]
    fn the_store_handle_is_the_store_the_runtime_reads() {
        let dir = temp_dir("store-handle");
        let runtime = CodeBuddyContractRuntime::new(&dir);
        let seed = runtime.load().unwrap();
        let handle = runtime.store_handle();
        assert!(Arc::ptr_eq(&handle, &runtime.store_handle()));

        let product = PINNED_PRODUCT_JSON.replace("\"Auto\"", "\"Auto (handle)\"");
        let candidate = seed
            .as_candidate()
            .with_catalog(product.as_bytes().to_vec(), "json");
        let published = handle.commit(&candidate).unwrap().expect("activación");
        assert_eq!(
            runtime.snapshot().unwrap().generation,
            published.generation,
            "el runtime debe ver la generación commiteada por el handle"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// El driver del runtime comparte la cache del runtime: el refresher no
    /// arranca con una segunda cache fría paralela.
    #[test]
    fn the_release_driver_shares_the_runtime_cache() {
        let dir = temp_dir("driver-cache");
        let runtime = CodeBuddyContractRuntime::new(&dir);
        let transport: Arc<dyn ContractTransport> = Arc::new(MapTransport::default());
        let driver: Arc<CodeBuddyReleaseDriver> = runtime.release_driver(transport);
        assert!(Arc::ptr_eq(driver.cache(), runtime.cache()));
        let _ = fs::remove_dir_all(&dir);
    }

    /// La raíz por defecto del LKG respeta el entorno.
    #[test]
    fn the_default_store_root_follows_the_environment() {
        let previous = std::env::var("OPENPROXY_CODEBUDDY_STORE_DIR").ok();
        // SAFETY: el test corre serializado por CODEBUDDY_TEST_LOCK, la misma
        // convención que el resto del workspace para mutar el entorno.
        let _guard = crate::spoofer::codebuddy::CODEBUDDY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        unsafe {
            std::env::set_var(
                "OPENPROXY_CODEBUDDY_STORE_DIR",
                "/tmp/openproxy-codebuddy-state-env",
            );
        }
        let root = default_store_root();
        match previous {
            Some(value) => unsafe {
                std::env::set_var("OPENPROXY_CODEBUDDY_STORE_DIR", value);
            },
            None => unsafe {
                std::env::remove_var("OPENPROXY_CODEBUDDY_STORE_DIR");
            },
        }
        assert_eq!(root, PathBuf::from("/tmp/openproxy-codebuddy-state-env"));
    }
}
