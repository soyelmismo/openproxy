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
use super::codex_state::shared::{ActivatableVersion, ContractProvider, ProviderRuntime};
use super::codex_state::{check_compiled_rules, default_store_root_for};
use crate::client_contract::{
    Candidate, ClientIdentity, ContractError, ContractId, ContractLimits, ContractTransport,
    ContractValidator, EvidenceKind, IDENTITY_ORIGIN_OFFICIAL, Snapshot, SourceRef, TransportCache,
    VersionChannel, hex_digest, parse_version,
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
        check_compiled_rules(
            candidate,
            CODEBUDDY_RUNTIME_RULE_SET_ID,
            CODEBUDDY_RUNTIME_EXTRACTOR_VERSION,
        )?;

        let version = parse_version(&candidate.payload.version)?;
        if version.channel != VersionChannel::Stable {
            return Err(ContractError::Rejected(format!(
                "la versión {:?} no es del canal estable",
                candidate.payload.version
            )));
        }
        // Without compiled guards for another revision, the pinned version is the
        // only activatable one.
        if candidate.payload.version != CODEBUDDY_VERSION {
            return Err(ContractError::Rejected(format!(
                "candidate announces version {:?} while compiled runtime pins {CODEBUDDY_VERSION}: \
                 without compiled guards for that revision candidate is rejected",
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

/// Piezas concretas del contrato CodeBuddy para [`ProviderRuntime`].
#[doc(hidden)]
pub struct CodeBuddyProvider;

impl ContractProvider for CodeBuddyProvider {
    type Profile = CodeBuddyProfile;
    type Driver = CodeBuddyReleaseDriver;

    const CONTRACT_ID: ContractId = ContractId::CodeBuddy;

    fn seed() -> Snapshot<CodeBuddyProfile> {
        embedded_snapshot()
    }

    fn validator() -> Arc<dyn ContractValidator<CodeBuddyProfile>> {
        Arc::new(CodeBuddyContractValidator)
    }

    fn driver(
        transport: Arc<dyn ContractTransport>,
        cache: Arc<TransportCache>,
        limits: ContractLimits,
    ) -> Arc<CodeBuddyReleaseDriver> {
        Arc::new(CodeBuddyReleaseDriver::new(transport, cache, limits))
    }

    async fn refresh(
        driver: Arc<CodeBuddyReleaseDriver>,
    ) -> Result<Option<Candidate<CodeBuddyProfile>>, ContractError> {
        driver.refresh().await
    }
}

/// La correspondencia paquete→commit del pin CodeBuddy no está verificada:
/// la versión anunciada solo se eleva cuando `identity_activatable` lo marca
/// el validador (fail-closed).
impl ActivatableVersion for CodeBuddyProfile {
    fn activatable_version(&self) -> Option<String> {
        self.identity_activatable.then(|| self.version.clone())
    }
}

/// Runtime del contrato CodeBuddy: delega en
/// [`GenericContractRuntime`](crate::client_contract::GenericContractRuntime) a
/// través del facade compartido [`ProviderRuntime`].
pub type CodeBuddyContractRuntime = ProviderRuntime<CodeBuddyProvider>;

/// Raíz por defecto del LKG en disco.
fn default_store_root() -> PathBuf {
    default_store_root_for("OPENPROXY_CODEBUDDY_STORE_DIR")
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
pub use super::codex_state::shared::refresh_validated_no_upstream as refresh_validated;

#[cfg(test)]
mod tests;
