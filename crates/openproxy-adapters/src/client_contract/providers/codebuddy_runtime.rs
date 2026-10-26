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
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::PackageCommitCorrespondence;
#[cfg(test)]
use super::codebuddy::{
    CODEBUDDY_DEPLOYMENT_TYPE, CODEBUDDY_ENDPOINT, CODEBUDDY_PLATFORM, CODEBUDDY_STAGING_ENDPOINT,
};
use super::codebuddy::{CODEBUDDY_PACKAGE, CODEBUDDY_VERSION, CodeBuddyCatalog, CodeBuddyContract};
use crate::client_contract::refresh::is_json_body;
use crate::client_contract::{
    CacheEntry, Candidate, ClientIdentity, ContractError, ContractId, ContractLimits,
    ContractTransport, EvidenceKind, FetchRequest, FetchResponse, IDENTITY_ORIGIN_OFFICIAL,
    SourceDescriptor, SourceRef, TransportCache, TransportCacheKey, hex_digest, parse_version,
    validate_catalog_body_size, validate_source_body_size,
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

/// La unidad que corresponde a bytes ya cacheados.
fn cached_unit(descriptor: &SourceDescriptor, entry: &CacheEntry) -> Acquired {
    Acquired {
        url: descriptor.url.clone(),
        body: entry.body.clone().unwrap_or_default(),
        digest: entry.digest.clone(),
        revision: descriptor.revision.clone(),
        evidence: descriptor.evidence,
    }
}

/// Mapea un estado no-2xx, no-304 al error del base.
fn status_error(url: &str, status: u16, response: &FetchResponse) -> ContractError {
    match status {
        404 | 410 => ContractError::SourceMissing {
            url: url.to_string(),
        },
        429 => ContractError::RateLimited {
            url: url.to_string(),
            retry_after: response.retry_after_secs(),
        },
        status if (500..=599).contains(&status) => ContractError::SourceUnavailable {
            url: url.to_string(),
            status,
        },
        status => ContractError::UnexpectedStatus {
            url: url.to_string(),
            status,
        },
    }
}

/// Adquisidor condicional sobre la infraestructura compartida.
pub struct CodeBuddyReleaseAcquirer {
    transport: Arc<dyn ContractTransport>,
    cache: Arc<TransportCache>,
    limits: ContractLimits,
    fresh: AtomicUsize,
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
            transport,
            cache,
            limits,
            fresh: AtomicUsize::new(0),
        }
    }

    /// Cuántos recursos devolvieron bytes frescos en esta ronda.
    #[must_use]
    pub fn fresh_count(&self) -> usize {
        self.fresh.load(Ordering::Relaxed)
    }

    /// Adquiere un recurso siguiendo la semántica condicional del base.
    ///
    /// Devuelve siempre bytes utilizables; un `304` sin bytes cacheados es
    /// error real y un cuerpo byte-idéntico al cacheado no cuenta como nuevo.
    ///
    /// # Errors
    /// Errores de transporte, de estado HTTP o de bytes cacheados ausentes.
    pub async fn acquire(&self, descriptor: &SourceDescriptor) -> Result<Acquired, ContractError> {
        let key =
            TransportCacheKey::new(&descriptor.url, &descriptor.accept, &descriptor.auth_scope);
        let cached = self.cache.get(&key);
        let request = self.build_request(descriptor, cached.as_ref());
        let response = self.transport.fetch(&request).await?;

        match response.status {
            200..=299 => self.handle_ok(descriptor, &key, cached, &response),
            304 => {
                self.handle_not_modified(descriptor, cached, &response)
                    .await
            }
            status => Err(status_error(&descriptor.url, status, &response)),
        }
    }

    /// Petición de un paso, con los validadores cacheados si son reusables.
    fn build_request(
        &self,
        descriptor: &SourceDescriptor,
        cached: Option<&CacheEntry>,
    ) -> FetchRequest {
        let mut headers = descriptor.headers.clone();
        headers.push(("Accept".to_string(), descriptor.accept.clone()));
        if let Some(entry) = cached.filter(|entry| entry.is_replayable()) {
            headers.extend(entry.conditional_headers());
        }
        FetchRequest {
            key: TransportCacheKey::new(
                &descriptor.url,
                &descriptor.accept,
                &descriptor.auth_scope,
            ),
            method: "GET".to_string(),
            url: descriptor.url.clone(),
            headers,
        }
    }

    /// Registra los bytes y validadores frescos para la próxima condicional.
    fn record(&self, key: &TransportCacheKey, response: &FetchResponse, digest: String) {
        self.cache.insert(CacheEntry {
            key: key.clone(),
            etag: response.etag(),
            last_modified: response.last_modified(),
            checked_at: chrono::Utc::now(),
            digest,
            bytes: response.body.len(),
            body: Some(response.body.clone()),
        });
    }

    /// Refresca los validadores de una entrada cacheada, conservando sus bytes.
    fn touch(&self, entry: CacheEntry, response: &FetchResponse) {
        let mut updated = entry;
        if let Some(etag) = response.etag() {
            updated.etag = Some(etag);
        }
        if let Some(last_modified) = response.last_modified() {
            updated.last_modified = Some(last_modified);
        }
        updated.checked_at = chrono::Utc::now();
        self.cache.insert(updated);
    }

    /// `2xx`: cuerpo fresco, o byte-idéntico al cacheado.
    fn handle_ok(
        &self,
        descriptor: &SourceDescriptor,
        key: &TransportCacheKey,
        cached: Option<CacheEntry>,
        response: &FetchResponse,
    ) -> Result<Acquired, ContractError> {
        validate_source_body_size(response.body.len(), &self.limits)?;
        let digest = hex_digest(&response.body);
        if let Some(entry) =
            cached.filter(|entry| entry.body_matches_digest() && entry.digest == digest)
        {
            // Mismo contenido, validadores nuevos: los bytes cacheados sirven.
            let unit = cached_unit(descriptor, &entry);
            self.touch(entry, response);
            return Ok(unit);
        }
        self.record(key, response, digest.clone());
        self.fresh.fetch_add(1, Ordering::Relaxed);
        Ok(Acquired {
            url: descriptor.url.clone(),
            body: response.body.clone(),
            digest,
            revision: descriptor.revision.clone(),
            evidence: descriptor.evidence,
        })
    }

    /// `304`: solo sirve si los bytes cacheados siguen intactos.
    async fn handle_not_modified(
        &self,
        descriptor: &SourceDescriptor,
        cached: Option<CacheEntry>,
        response: &FetchResponse,
    ) -> Result<Acquired, ContractError> {
        if let Some(entry) = cached.filter(CacheEntry::body_matches_digest) {
            let unit = cached_unit(descriptor, &entry);
            self.touch(entry, response);
            return Ok(unit);
        }
        // Sin bytes usables un `304` no prueba nada: un reintento
        // incondicional, y un segundo `304` se rechaza.
        let retry = self.build_request(descriptor, None);
        let retried = self.transport.fetch(&retry).await?;
        match retried.status {
            200..=299 => self.handle_ok(descriptor, &retry.key, None, &retried),
            304 => Err(ContractError::MissingCachedBytes {
                url: descriptor.url.clone(),
            }),
            status => Err(status_error(&descriptor.url, status, &retried)),
        }
    }
}

/// Metadata mutable del registro npm, tal como la lee el driver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryMetadata {
    /// Versión anunciada por `latest`.
    pub version: String,
    /// `dist.integrity` declarada por el registro, si existe.
    pub integrity: Option<String>,
}

/// Parsea la metadata mutable del registro.
///
/// # Errors
/// Rechaza un cuerpo que no sea JSON, sin `version`, o con una versión no
/// semver de tres partes.
pub fn parse_registry_metadata(body: &[u8]) -> Result<RegistryMetadata, ContractError> {
    if !is_json_body(body) {
        return Err(ContractError::Rejected(
            "registry/latest no devolvió JSON".to_string(),
        ));
    }
    let value: Value = serde_json::from_slice(body).map_err(ContractError::from)?;
    let version = value
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| ContractError::Rejected("registry/latest sin campo `version`".to_string()))?
        .to_string();
    parse_version(&version)?;
    let integrity = value
        .get("dist")
        .and_then(|dist| dist.get("integrity"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(RegistryMetadata { version, integrity })
}

/// `package.json` del paquete fijado, en lo que el contrato usa.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageManifest {
    /// Nombre del paquete.
    pub name: String,
    /// Versión declarada.
    pub version: String,
}

/// Parsea el `package.json` del paquete.
///
/// # Errors
/// Rechaza un cuerpo que no sea JSON o sin `name`/`version` de texto.
pub fn parse_package_manifest(body: &[u8]) -> Result<PackageManifest, ContractError> {
    if !is_json_body(body) {
        return Err(ContractError::Rejected(
            "package.json no devolvió JSON".to_string(),
        ));
    }
    let value: Value = serde_json::from_slice(body).map_err(ContractError::from)?;
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ContractError::Rejected("package.json sin campo `name`".to_string()))?
        .to_string();
    let version = value
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| ContractError::Rejected("package.json sin campo `version`".to_string()))?
        .to_string();
    parse_version(&version)?;
    Ok(PackageManifest { name, version })
}

/// Campos de `product.json` que el contrato publica.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProductDocument {
    /// Endpoint de producción.
    pub endpoint: String,
    /// Endpoint de staging.
    pub staging_endpoint: String,
    /// Tipo de despliegue.
    pub deployment_type: String,
    /// Plataforma.
    pub platform: String,
    /// Endpoints oficiales, si el documento los declara como lista de textos.
    pub official_endpoints: Vec<String>,
}

/// Extrae los campos publicados de `product.json`.
///
/// Un campo de texto ausente o de otro tipo se rechaza: el contrato no rellena
/// huecos con valores por defecto.
///
/// # Errors
/// [`ContractError::Rejected`] si falta un campo obligatorio o no es texto.
pub fn parse_product_document(body: &[u8]) -> Result<ProductDocument, ContractError> {
    if !is_json_body(body) {
        return Err(ContractError::Rejected(
            "product.json no devolvió JSON".to_string(),
        ));
    }
    let value: Value = serde_json::from_slice(body).map_err(ContractError::from)?;
    let text = |field: &str| -> Result<String, ContractError> {
        value
            .get(field)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                ContractError::Rejected(format!("product.json sin campo de texto `{field}`"))
            })
    };
    let official_endpoints = value
        .get("officialEndpoints")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(ProductDocument {
        endpoint: text("endpoint")?,
        staging_endpoint: text("stagingEndpoint")?,
        deployment_type: text("deploymentType")?,
        platform: text("platform")?,
        official_endpoints,
    })
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
    transport: Arc<dyn ContractTransport>,
    cache: Arc<TransportCache>,
    limits: ContractLimits,
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
            transport,
            cache,
            limits,
        }
    }

    /// La cache compartida, para inspección o precalentamiento.
    #[must_use]
    pub fn cache(&self) -> &Arc<TransportCache> {
        &self.cache
    }

    /// Un adquisidor nuevo sobre la misma infraestructura.
    fn acquirer(&self) -> CodeBuddyReleaseAcquirer {
        CodeBuddyReleaseAcquirer::new(
            Arc::clone(&self.transport),
            Arc::clone(&self.cache),
            self.limits,
        )
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
        validate_catalog_body_size(product_unit.body.len(), &self.limits)?;
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
        if catalog.models.len() > self.limits.catalog_models {
            return Err(ContractError::CatalogTooLarge {
                models: catalog.models.len(),
                limit: self.limits.catalog_models,
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
mod tests {
    #![allow(clippy::pedantic, clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    /// Transporte que responde desde un mapa de URL. Ninguna petición sale del
    /// proceso; una URL ausente se comporta como `404`.
    #[derive(Default)]
    struct MapTransport {
        responses: Mutex<HashMap<String, FetchResponse>>,
        seen: Mutex<Vec<FetchRequest>>,
    }

    impl MapTransport {
        fn with(entries: Vec<(String, FetchResponse)>) -> Self {
            let mut responses = HashMap::new();
            for (url, response) in entries {
                responses.insert(url, response);
            }
            Self {
                responses: Mutex::new(responses),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn set(&self, url: String, response: FetchResponse) {
            self.responses.lock().unwrap().insert(url, response);
        }

        fn requests(&self) -> Vec<FetchRequest> {
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

    fn fixture_bytes(relative: &str) -> Vec<u8> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
        std::fs::read(root.join(relative))
            .unwrap_or_else(|error| panic!("fixture {relative}: {error}"))
    }

    fn limits() -> ContractLimits {
        ContractLimits {
            catalog_bytes: 8 * 1024 * 1024,
            source_bytes: 8 * 1024 * 1024,
            ..ContractLimits::default()
        }
    }

    fn driver(transport: MapTransport) -> CodeBuddyReleaseDriver {
        CodeBuddyReleaseDriver::new(
            Arc::new(transport),
            Arc::new(TransportCache::new()),
            limits(),
        )
    }

    fn ok_json(body: &[u8]) -> FetchResponse {
        FetchResponse {
            status: 200,
            headers: vec![("etag".to_string(), "\"v1\"".to_string())],
            body: body.to_vec(),
        }
    }

    /// Mapa de red del pin completo: los tres recursos con los fixtures reales.
    fn pin_map(announced: &str) -> MapTransport {
        let registry = fixture_bytes("codebuddy/registry-latest.json");
        let registry_body = if announced == CODEBUDDY_VERSION {
            registry
        } else {
            format!(
                r#"{{"name":"{CODEBUDDY_PACKAGE}","version":"{announced}","dist":{{"integrity":"sha512-x"}}}}"#
            )
            .into_bytes()
        };
        MapTransport::with(vec![
            (
                CODEBUDDY_REGISTRY_LATEST_URL.to_string(),
                ok_json(&registry_body),
            ),
            (
                pinned_package_url(CODEBUDDY_VERSION),
                ok_json(&fixture_bytes("codebuddy-extracted/package.json")),
            ),
            (
                pinned_product_url(CODEBUDDY_VERSION),
                ok_json(&fixture_bytes("codebuddy-extracted/product.json")),
            ),
        ])
    }

    /// El refresh del pin real construye un candidato con catálogo de 22
    /// modelos y sin identidad versionada: la correspondencia no está
    /// verificada y el paquete npm no expone `gitHead`.
    #[tokio::test]
    async fn a_pinned_round_builds_a_catalog_candidate_without_identity_elevation() {
        let driver = driver(pin_map(CODEBUDDY_VERSION));
        let candidate = driver.refresh().await.unwrap().expect("candidato");
        assert_eq!(candidate.contract_id, ContractId::CodeBuddy);
        assert_eq!(candidate.payload.version, CODEBUDDY_VERSION);
        assert_eq!(candidate.payload.package, CODEBUDDY_PACKAGE);
        assert_eq!(candidate.payload.catalog_models, 22);
        assert!(!candidate.payload.identity_activatable);
        assert!(
            candidate.client_identity.version.is_none(),
            "sin correspondencia verificada la identidad no lleva versión"
        );
        assert!(candidate.catalog_body().is_some());
        // Tres fuentes: metadata, paquete y producto, cada una con su digest.
        assert_eq!(candidate.sources.len(), 3);
        assert!(candidate.sources.iter().any(|source| {
            source.evidence == EvidenceKind::PackageManifest
                && source.revision.as_deref() == Some(CODEBUDDY_VERSION)
        }));
        assert!(candidate.sources.iter().any(|source| {
            source.evidence == EvidenceKind::RuntimeCatalog
                && source.revision.as_deref() == Some(CODEBUDDY_VERSION)
        }));
    }

    /// Una metadata que anuncia otra versión se rechaza: la metadata sola no
    /// eleva la identidad ni el catálogo del pin.
    #[tokio::test]
    async fn a_newer_announced_version_is_rejected_because_metadata_alone_never_elevates() {
        let driver = driver(pin_map("2.165.0"));
        let error = driver.refresh().await.unwrap_err();
        assert!(
            matches!(error, ContractError::Rejected(ref message) if message.contains("metadata alone")),
            "got {error:?}"
        );
    }

    /// Una ronda con todos los bytes idénticos a los cacheados no produce
    /// candidato: la generación activa sigue en pie.
    #[tokio::test]
    async fn an_identical_round_is_unchanged() {
        let driver = driver(pin_map(CODEBUDDY_VERSION));
        let first = driver.refresh().await.unwrap().expect("primera ronda");
        let second = driver.refresh().await.unwrap();
        assert!(second.is_none(), "bytes idénticos no son una ronda nueva");
        assert_eq!(first.payload.catalog_models, 22);
    }

    /// Un `package.json` cuya versión no es la del pin se rechaza: el catálogo
    /// y la identidad no se pueden unir entre revisiones distintas.
    #[tokio::test]
    async fn a_package_manifest_from_another_revision_is_rejected() {
        let transport = pin_map(CODEBUDDY_VERSION);
        transport.set(
            pinned_package_url(CODEBUDDY_VERSION),
            ok_json(format!(r#"{{"name":"{CODEBUDDY_PACKAGE}","version":"2.157.0"}}"#).as_bytes()),
        );
        let driver = driver(transport);
        let error = driver.refresh().await.unwrap_err();
        assert!(
            matches!(error, ContractError::Rejected(ref message) if message.contains("package.json")),
            "got {error:?}"
        );
    }

    /// Un `product.json` con un vendor desconocido se rechaza: el parser no
    /// relaja enums para adoptar una revisión nueva.
    #[tokio::test]
    async fn a_product_document_with_an_unknown_vendor_is_rejected() {
        let transport = pin_map(CODEBUDDY_VERSION);
        let product = String::from_utf8(fixture_bytes("codebuddy-extracted/product.json"))
            .unwrap()
            .replace("\"vendor\": \"e\"", "\"vendor\": \"zz\"");
        transport.set(
            pinned_product_url(CODEBUDDY_VERSION),
            ok_json(product.as_bytes()),
        );
        let driver = driver(transport);
        let error = driver.refresh().await.unwrap_err();
        assert!(
            matches!(error, ContractError::Rejected(ref message) if message.contains("catálogo")),
            "got {error:?}"
        );
    }

    /// La adquisición es condicional de verdad: la segunda ronda reenvía los
    /// validadores cacheados.
    #[tokio::test]
    async fn the_second_round_sends_conditional_headers() {
        let transport = Arc::new(pin_map(CODEBUDDY_VERSION));
        let driver = CodeBuddyReleaseDriver::new(
            Arc::<MapTransport>::clone(&transport),
            Arc::new(TransportCache::new()),
            limits(),
        );
        let _ = driver.refresh().await.unwrap();
        let first_requests = transport.requests().len();
        let _ = driver.refresh().await.unwrap();
        let later = &transport.requests()[first_requests..];
        assert!(
            later.iter().all(|request| request
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("if-none-match"))),
            "las rondas posteriores deben llevar If-None-Match"
        );
    }

    /// La procedencia declarada nunca promueve la correspondencia por sí sola.
    #[test]
    fn correspondence_stays_unverified_from_evidence_alone() {
        let sources = vec![SourceRef::pinned(
            pinned_product_url(CODEBUDDY_VERSION),
            CODEBUDDY_VERSION,
            hex_digest(b"{}"),
            EvidenceKind::RuntimeCatalog,
        )];
        let correspondence = correspondence_from_evidence(&sources);
        assert!(!correspondence.is_verified());
    }

    /// Los campos publicados del producto salen del documento, no de
    /// constantes: el fixture declara los mismos valores que el pin.
    #[test]
    fn the_product_document_fields_come_from_the_fixture() {
        let document =
            parse_product_document(&fixture_bytes("codebuddy-extracted/product.json")).unwrap();
        assert_eq!(document.endpoint, CODEBUDDY_ENDPOINT);
        assert_eq!(document.staging_endpoint, CODEBUDDY_STAGING_ENDPOINT);
        assert_eq!(document.deployment_type, CODEBUDDY_DEPLOYMENT_TYPE);
        assert_eq!(document.platform, CODEBUDDY_PLATFORM);
    }
}
