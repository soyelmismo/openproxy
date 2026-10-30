//! Adquisición pinned/condicional del contrato CodeBuddy (npm `@tencent-ai/codebuddy-code`).
//!
//! Este módulo es el equivalente CodeBuddy de [`super::codex_runtime`]: adquiere
//! los bytes del paquete **fijado** sobre la infraestructura compartida
//! ([`TransportCache`] condicional, [`ContractLimits`], [`ContractTransport`]) y
//! construye un [`Candidate`] tipado. No persiste nada, no valida el contrato
//! final y no ejecuta el bundle: eso es del store, del validador
//! ([`super::codebuddy_state::CodeBuddyContractValidator`]) y del parser
//! ([`super::codebuddy`]).
//!
//! ## Regla de identidad (fail-closed)
//!
//! El paquete npm **no** expone `gitHead`: no hay campo inmutable que ate el
//! artefacto publicado a un commit. La correspondencia paquete→commit queda
//! [`PackageCommitCorrespondence::Unverified`] y, por tanto,
//! [`super::codebuddy::CodeBuddyContract::identity_activatable`] es `false`.
//! Consecuencias, aplicadas aquí y re-verificadas por el validador:
//!
//! * La **identidad no se eleva** con metadata sola: una metadata que anuncie
//!   una versión distinta de la fijada se rechaza ([`ContractError::Rejected`])
//!   y la generación activa permanece.
//! * Tampoco se eleva con el paquete solo: aunque `package.json` y
//!   `product.json` de otra revisión se descarguen, sin guards compilados para
//!   esa revisión el candidato se rechaza.
//! * El catálogo sí se publica como **overlay separado** (el snapshot lleva el
//!   `product.json` verificado por digest), sin fingir que la versión de
//!   identidad del cliente cambió.
//!
//! ## Adquisición condicional, sin descargar el bundle
//!
//! Cada recurso pasa por [`CodeBuddyReleaseAcquirer`]: `ETag`/`Last-Modified`
//! cacheados se reenvían como `If-None-Match`/`If-Modified-Since`, un `304`
//! solo se acepta con bytes cacheados intactos y un cuerpo byte-idéntico al
//! cacheado no cuenta como fresco. El bundle `dist/codebuddy.js` (19 MB) **no**
//! se descarga en la ronda: los guards de offset de
//! [`super::codebuddy::verify_bundle_guards`] se aplican cuando hay bytes del
//! bundle (p. ej. desde el fixture capturado), nunca ejecutando el artefacto.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::PackageCommitCorrespondence;
#[cfg(test)]
use super::codebuddy::{
    CODEBUDDY_DEPLOYMENT_TYPE, CODEBUDDY_ENDPOINT, CODEBUDDY_PLATFORM, CODEBUDDY_STAGING_ENDPOINT,
};
use super::codebuddy::{CODEBUDDY_PACKAGE, CODEBUDDY_VERSION, CodeBuddyCatalog, CodeBuddyContract};
use super::codex_runtime::acquirer::{AcquirerCore, ReleaseInfra};
use crate::client_contract::{
    Candidate, ClientIdentity, ContractError, ContractId, ContractLimits, ContractTransport,
    EvidenceKind, IDENTITY_ORIGIN_OFFICIAL, SourceDescriptor, SourceRef, TransportCache,
    hex_digest, parse_version, validate_catalog_body_size,
};

mod metadata;

pub use metadata::{
    PackageManifest, ProductDocument, RegistryMetadata, parse_package_manifest,
    parse_product_document, parse_registry_metadata,
};

/// Identidad de las reglas compiladas de extracción y guards de CodeBuddy.
pub const CODEBUDDY_RUNTIME_RULE_SET_ID: &str = "codebuddy-npm-pinned-v1";

/// Versión del extractor compilado.
pub const CODEBUDDY_RUNTIME_EXTRACTOR_VERSION: u32 = 1;

/// Metadata mutable del paquete npm (nunca identidad por sí sola).
pub const CODEBUDDY_REGISTRY_LATEST_URL: &str =
    "https://registry.npmjs.org/@tencent-ai/codebuddy-code/latest";

/// Prefijo de unpkg para una revisión fijada del paquete.
pub const CODEBUDDY_UNPKG_PREFIX: &str = "https://unpkg.com/@tencent-ai/codebuddy-code";

/// `Accept` de las fuentes JSON del contrato.
const ACCEPT_JSON: &str = "application/json";

/// Perfil CodeBuddy activado: lo que un snapshot publica y lo que el adapter
/// puede leer en el hot path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeBuddyProfile {
    /// Nombre del paquete npm (`@tencent-ai/codebuddy-code`).
    pub package: String,
    /// Versión del paquete fijado (`2.164.0`).
    pub version: String,
    /// Endpoint de producción declarado por `product.json`.
    pub endpoint: String,
    /// Endpoint de staging declarado por `product.json`.
    pub staging_endpoint: String,
    /// Tipo de despliegue declarado (`SaaS`).
    pub deployment_type: String,
    /// Plataforma declarada (`CLI`).
    pub platform: String,
    /// Endpoints oficiales declarados por `product.json`, en orden.
    pub official_endpoints: Vec<String>,
    /// Modelos del catálogo verificado.
    pub catalog_models: usize,
    /// Si la correspondencia paquete→commit permite activar identidad.
    /// En `2.164.0` es `false`: el paquete npm no expone `gitHead`.
    pub identity_activatable: bool,
    /// Motivo auditable del estado de correspondencia.
    pub correspondence_reason: String,
}

impl CodeBuddyProfile {
    /// La correspondencia derivada del perfil, en la forma del parser.
    #[must_use]
    pub fn correspondence(&self) -> PackageCommitCorrespondence {
        if self.identity_activatable {
            // Ninguna ruta de evidencia disponible hoy produce esto; si algún
            // día ocurre, la procedencia debe venir de un commit completo.
            PackageCommitCorrespondence::Verified {
                tag: format!("npm-{}", self.version),
                commit: String::new(),
            }
        } else {
            PackageCommitCorrespondence::Unverified {
                reason: self.correspondence_reason.clone(),
            }
        }
    }
}

/// Estado de correspondencia derivado de la **evidencia disponible**, no del
/// payload.
///
/// El paquete npm de CodeBuddy no expone `gitHead` y los mirrors (unpkg) no
/// añaden un campo inmutable que ate paquete→commit, así que la evidencia de
/// esta ronda siempre es [`PackageCommitCorrespondence::Unverified`]. El
/// validador recomputa esto sobre las fuentes del candidato: un payload que
/// declare identidad activable sin evidencia nueva se rechaza.
#[must_use]
pub fn correspondence_from_evidence(_sources: &[SourceRef]) -> PackageCommitCorrespondence {
    PackageCommitCorrespondence::Unverified {
        reason: "el paquete npm no expone gitHead: sin correspondencia paquete→commit verificable"
            .to_string(),
    }
}

/// URL de `package.json` para una revisión fijada.
#[must_use]
pub fn pinned_package_url(version: &str) -> String {
    format!("{CODEBUDDY_UNPKG_PREFIX}@{version}/package.json")
}

/// URL de `product.json` para una revisión fijada.
#[must_use]
pub fn pinned_product_url(version: &str) -> String {
    format!("{CODEBUDDY_UNPKG_PREFIX}@{version}/product.json")
}

/// Un descriptor de fuente del contrato, con su evidencia y revisión.
fn step(
    url: String,
    accept: &str,
    evidence: EvidenceKind,
    revision: Option<String>,
) -> SourceDescriptor {
    SourceDescriptor {
        url,
        accept: accept.to_string(),
        auth_scope: "public".to_string(),
        headers: Vec::new(),
        evidence,
        revision,
    }
}

/// Una fuente adquirida, con su digest y procedencia.
#[derive(Clone, Debug)]
pub struct Acquired {
    /// URL de la que salieron los bytes.
    pub url: String,
    /// Bytes utilizables (frescos o cacheados).
    pub body: Vec<u8>,
    /// SHA-256 hex de `body`.
    pub digest: String,
    /// Revisión fijada de la fuente, si la tiene.
    pub revision: Option<String>,
    /// Tipo de evidencia que los bytes representan.
    pub evidence: EvidenceKind,
}

impl Acquired {
    /// La referencia de procedencia de esta unidad.
    #[must_use]
    pub fn source_ref(&self) -> SourceRef {
        SourceRef::pinned(
            self.url.clone(),
            self.revision.clone().unwrap_or_default(),
            self.digest.clone(),
            self.evidence,
        )
    }
}

/// Adquisidor condicional sobre la infraestructura compartida.
///
/// La mecánica condicional (cache, límites, `304` con bytes cacheados,
/// `404`/`410`/`429`/`5xx` a los errores del base) es la de
/// [`AcquirerCore`], compartida con [`super::codex_runtime::CodexReleaseAcquirer`]:
/// solo cambia el tipo de la unidad `Acquired`.
pub struct CodeBuddyReleaseAcquirer {
    inner: AcquirerCore,
}

impl CodeBuddyReleaseAcquirer {
    /// Construye un adquisidor sobre infraestructura compartida.
    #[must_use]
    pub fn new(
        transport: Arc<dyn ContractTransport>,
        cache: Arc<TransportCache>,
        limits: ContractLimits,
    ) -> Self {
        Self {
            inner: AcquirerCore::new(transport, cache, limits),
        }
    }

    /// Cuántos recursos devolvieron bytes frescos en esta ronda.
    #[must_use]
    pub fn fresh_count(&self) -> usize {
        self.inner.fresh_count()
    }

    /// Adquiere un recurso siguiendo la semántica condicional del base.
    ///
    /// Devuelve siempre bytes utilizables; un `304` sin bytes cacheados es
    /// error real y un cuerpo byte-idéntico al cacheado no cuenta como nuevo.
    ///
    /// # Errors
    /// Errores de transporte, de estado HTTP o de bytes cacheados ausentes.
    pub async fn acquire(&self, descriptor: &SourceDescriptor) -> Result<Acquired, ContractError> {
        self.inner
            .acquire(descriptor, |descriptor, body, digest| Acquired {
                url: descriptor.url.clone(),
                body,
                digest,
                revision: descriptor.revision.clone(),
                evidence: descriptor.evidence,
            })
            .await
    }
}

/// El pin resuelto de una ronda: revisión fijada y lo que anunció la metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedCodeBuddyPin {
    /// Versión fijada por el runtime compilado.
    pub pinned_version: String,
    /// Versión que anunció `registry/latest` en esta ronda.
    pub announced_version: String,
    /// Integridad declarada por el registro, si existe.
    pub announced_integrity: Option<String>,
}

/// Orquestación async del pin completo de CodeBuddy.
///
/// Encadena los saltos y construye el [`Candidate`] final. No duplica refresh
/// ni validación: cada salto pasa por [`CodeBuddyReleaseAcquirer`] y la forma,
/// los límites y los secretos los verifica el base al commitear.
pub struct CodeBuddyReleaseDriver {
    inner: ReleaseInfra,
}

impl CodeBuddyReleaseDriver {
    /// Construye el driver sobre infraestructura compartida.
    #[must_use]
    pub fn new(
        transport: Arc<dyn ContractTransport>,
        cache: Arc<TransportCache>,
        limits: ContractLimits,
    ) -> Self {
        Self {
            inner: ReleaseInfra::new(transport, cache, limits),
        }
    }

    /// La cache compartida, para inspección o precalentamiento.
    #[must_use]
    pub fn cache(&self) -> &Arc<TransportCache> {
        self.inner.cache()
    }

    /// Un adquisidor nuevo sobre la misma infraestructura.
    fn acquirer(&self) -> CodeBuddyReleaseAcquirer {
        CodeBuddyReleaseAcquirer {
            inner: self.inner.acquirer_core(),
        }
    }

    /// Resuelve el pin partiendo de la metadata mutable.
    ///
    /// La metadata **no** eleva la identidad: si `latest` anuncia una versión
    /// distinta de [`CODEBUDDY_VERSION`], no hay guards compilados para esa
    /// revisión y el candidato se rechaza en lugar de adoptarse. El catálogo de
    /// la revisión nueva puede publicarse como overlay cuando existan reglas
    /// compiladas para ella; hoy no existen.
    ///
    /// # Errors
    /// Errores de transporte/parseo, o [`ContractError::Rejected`] cuando la
    /// versión anunciada no es la fijada.
    pub async fn resolve_pin(&self) -> Result<ResolvedCodeBuddyPin, ContractError> {
        let acquirer = self.acquirer();
        self.resolve_pin_with(&acquirer).await
    }

    /// Construye el candidato completo del pin.
    ///
    /// Verifica, sobre los bytes de **la misma revisión**: el `package.json`
    /// del paquete fijado, el `product.json` del que sale el catálogo, y el
    /// estado de correspondencia paquete→commit derivado de la evidencia.
    /// El catálogo viaja como `catalog_bytes` separado del perfil pequeño.
    ///
    /// # Errors
    /// Errores de transporte/parseo o [`ContractError::Rejected`] si el
    /// paquete o el producto no son los del pin.
    pub async fn build_candidate(
        &self,
        pin: &ResolvedCodeBuddyPin,
    ) -> Result<Candidate<CodeBuddyProfile>, ContractError> {
        let acquirer = self.acquirer();
        self.build_candidate_with(&acquirer, pin).await
    }

    /// Variante interna que reutiliza el adquisidor de la ronda.
    async fn build_candidate_with(
        &self,
        acquirer: &CodeBuddyReleaseAcquirer,
        pin: &ResolvedCodeBuddyPin,
    ) -> Result<Candidate<CodeBuddyProfile>, ContractError> {
        let version = pin.pinned_version.clone();
        let mut sources: Vec<SourceRef> = vec![SourceRef::pinned(
            CODEBUDDY_REGISTRY_LATEST_URL,
            pin.announced_version.clone(),
            hex_digest(pin.announced_version.as_bytes()),
            EvidenceKind::ReleaseMetadata,
        )];

        // `package.json` de la revisión fijada: el paquete y la versión deben
        // coincidir con el pin.
        let package_unit = acquirer
            .acquire(&step(
                pinned_package_url(&version),
                ACCEPT_JSON,
                EvidenceKind::PackageManifest,
                Some(version.clone()),
            ))
            .await?;
        let manifest = parse_package_manifest(&package_unit.body)?;
        if manifest.name != CODEBUDDY_PACKAGE {
            return Err(ContractError::Rejected(format!(
                "el paquete del pin declara {:?} y se esperaba {CODEBUDDY_PACKAGE:?}",
                manifest.name
            )));
        }
        if manifest.version != version {
            return Err(ContractError::Rejected(format!(
                "el package.json del pin declara la versión {:?} y el pin es {:?}",
                manifest.version, version
            )));
        }
        sources.push(package_unit.source_ref());

        // `product.json` de la revisión fijada: catálogo tipado y campos
        // publicados, verificados por digest.
        let product_unit = acquirer
            .acquire(&step(
                pinned_product_url(&version),
                ACCEPT_JSON,
                EvidenceKind::RuntimeCatalog,
                Some(version.clone()),
            ))
            .await?;
        validate_catalog_body_size(product_unit.body.len(), self.inner.limits())?;
        let product_text = std::str::from_utf8(&product_unit.body)
            .map_err(|_| ContractError::Rejected("product.json no es UTF-8 válido".to_string()))?;
        // Un catálogo corrupto o con un enum no soportado es rechazo, nunca
        // LKG silencioso.
        let catalog = CodeBuddyCatalog::parse(product_text)
            .map_err(|error| ContractError::Rejected(format!("catálogo del pin: {error}")))?;
        if catalog.models.is_empty() {
            return Err(ContractError::Rejected(
                "el catálogo del pin no declara modelos".to_string(),
            ));
        }
        if catalog.models.len() > self.inner.limits().catalog_models {
            return Err(ContractError::CatalogTooLarge {
                models: catalog.models.len(),
                limit: self.inner.limits().catalog_models,
            });
        }
        let document = parse_product_document(&product_unit.body)?;
        sources.push(product_unit.source_ref());

        // Correspondencia derivada de la evidencia (no del payload): sin
        // `gitHead` no hay identidad activable.
        let correspondence = correspondence_from_evidence(&sources);
        let identity_activatable = CodeBuddyContract {
            catalog,
            correspondence: correspondence.clone(),
        }
        .identity_activatable();
        let correspondence_reason = match correspondence {
            PackageCommitCorrespondence::Unverified { reason } => reason,
            PackageCommitCorrespondence::Verified { .. } => String::new(),
        };

        let catalog_models = CodeBuddyCatalog::parse(product_text)
            .map_err(|error| ContractError::Rejected(format!("catálogo del pin: {error}")))?
            .models
            .len();

        let profile = CodeBuddyProfile {
            package: manifest.name,
            version: version.clone(),
            endpoint: document.endpoint,
            staging_endpoint: document.staging_endpoint,
            deployment_type: document.deployment_type,
            platform: document.platform,
            official_endpoints: document.official_endpoints,
            catalog_models,
            identity_activatable,
            correspondence_reason,
        };

        // La identidad solo lleva versión cuando la correspondencia está
        // verificada; hoy no lo está, así que el snapshot anuncia identidad sin
        // versión y el catálogo viaja como overlay separado.
        let identity = if identity_activatable {
            ClientIdentity::versioned(version, IDENTITY_ORIGIN_OFFICIAL)
        } else {
            ClientIdentity::unversioned(IDENTITY_ORIGIN_OFFICIAL)
        };

        Ok(Candidate::without_catalog(
            ContractId::CodeBuddy,
            identity,
            CODEBUDDY_RUNTIME_RULE_SET_ID,
            CODEBUDDY_RUNTIME_EXTRACTOR_VERSION,
            sources,
            profile,
        )
        .with_catalog(product_unit.body, "json"))
    }

    /// Una ronda completa: resuelve el pin y construye el candidato.
    ///
    /// Devuelve `Ok(None)` cuando **ningún** recurso del pin trajo bytes
    /// frescos: la generación activa sigue en pie y no hay nada que commitear.
    ///
    /// # Errors
    /// Errores del pin o de la construcción del candidato.
    pub async fn refresh(&self) -> Result<Option<Candidate<CodeBuddyProfile>>, ContractError> {
        let acquirer = self.acquirer();
        let pin = self.resolve_pin_with(&acquirer).await?;
        let candidate = self.build_candidate_with(&acquirer, &pin).await?;
        if acquirer.fresh_count() == 0 {
            return Ok(None);
        }
        Ok(Some(candidate))
    }

    /// Resuelve el pin reutilizando un adquisidor concreto.
    async fn resolve_pin_with(
        &self,
        acquirer: &CodeBuddyReleaseAcquirer,
    ) -> Result<ResolvedCodeBuddyPin, ContractError> {
        let metadata_unit = acquirer
            .acquire(&step(
                CODEBUDDY_REGISTRY_LATEST_URL.to_string(),
                ACCEPT_JSON,
                EvidenceKind::ReleaseMetadata,
                None,
            ))
            .await?;
        let metadata = parse_registry_metadata(&metadata_unit.body)?;
        let announced = parse_version(&metadata.version)?;
        if announced.channel != crate::client_contract::VersionChannel::Stable {
            return Err(ContractError::Rejected(format!(
                "registry/latest announces version {:?}, which is not from stable channel",
                metadata.version
            )));
        }
        if metadata.version != CODEBUDDY_VERSION {
            return Err(ContractError::Rejected(format!(
                "registry/latest announces {} while compiled runtime pins {}: \
                 without compiled guards for the new revision, metadata alone \
                 does not elevate identity and candidate is rejected",
                metadata.version, CODEBUDDY_VERSION
            )));
        }
        Ok(ResolvedCodeBuddyPin {
            pinned_version: CODEBUDDY_VERSION.to_string(),
            announced_version: metadata.version,
            announced_integrity: metadata.integrity,
        })
    }
}

#[cfg(test)]
mod tests;
