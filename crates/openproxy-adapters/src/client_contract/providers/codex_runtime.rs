//! Driver de adquisición release-pinned del contrato Codex.
//!
//! Flujo completo, en una sola ronda orquestada y reutilizando la
//! infraestructura común del base ([`ContractTransport`], [`TransportCache`],
//! [`ContractLimits`], validación estructural y guards de región):
//!
//! 1. `releases/latest` (mutable, `EvidenceKind::ReleaseMetadata`) → tag
//!    estable `rust-vX.Y.Z`, nunca `prerelease` ni `draft`.
//! 2. `git/refs/tags/<tag>` → objeto tag anotado.
//! 3. `git/tags/<sha>` → commit, con desreferenciación **recursiva y acotada**
//!    ([`MAX_TAG_DEREFERENCES`]): una cadena de tags anidados nunca se sigue
//!    más allá del límite.
//! 4. `raw.githubusercontent.com/openai/codex/<commit>/codex-rs/Cargo.toml`
//!    ([`EvidenceKind::PackageManifest`]) → la versión de `[workspace.package]`
//!    debe coincidir con la versión anunciada por la release.
//! 5. Archivos `.rs` raw del **mismo commit** ([`EvidenceKind::BundleHash`]) →
//!    verificación de [`super::codex::CODEX_REGION_GUARDS`] con el verificador
//!    ya existente, y parse del catálogo tipado.
//! 6. Catálogo `models.json` del mismo commit → `catalog_bytes` + `catalog_ref`,
//!    **separado** del perfil pequeño (nunca inlineado en el envelope).
//!
//! Reglas duras:
//!
//! * Ninguna integración con adapters ni spoofer: este módulo solo produce
//!   [`Candidate<CodexProfile>`]. El integrador decide qué hacer con ella.
//! * No hay refresh duplicado: cada salto es una única petición condicional
//!   sobre el transporte compartido, con la misma cache, límites y semántica
//!   `304`/`404`/`410`/`429`/`5xx` que el base. El driver no vuelve a pedir un
//!   cuerpo que la cache ya tiene, y no reimplementa la validación estructural.
//! * Los nombres de header de sesión/thread **no** viajan en el perfil: son
//!   *compiled rules* verificadas por guards sobre los bytes del pin. El perfil
//!   declara qué se verificó y contra qué revisión, no el detalle de wire.
//! * Reglas compiladas desconocidas (guard ausente o duplicado, enum de
//!   catálogo no soportado, versión que no coincide, tag fuera del pin) ⇒
//!   `Err`. Nunca se adopta una revisión nueva sin guards para ella: el LKG del
//!   store permanece intacto.
//!
//! ## Por qué existe orquestación async fuera de `ContractDriver`
//!
//! [`ContractDriver`] es `fn source(&self) -> SourceDescriptor` +
//! `fn parse(&self, body, revision, digest)`: una sola fuente, síncrona. El pin
//! de Codex es inherentemente multi-salto (latest → tag → commit → varios raw).
//! No caben fetches anidados dentro de `parse` sin bloquear el runtime Tokio ni
//! invocar un subprocess. Por eso [`CodexReleaseDriver`] es un orquestador
//! async que **reutiliza** la adquisición común ([`CodexReleaseAcquirer`], el
//! mismo transporte, la misma cache y los mismos límites) en vez de
//! reimplementarla. Además, el último salto se expone como
//! [`CodexPinnedRawDriver`], que sí implementa [`ContractDriver`], de modo que
//! el `ContractRefresher` del base siga siendo dueño de la petición condicional
//! cuando el caller ya resolvió el commit.
//!
//! ## Nota sobre el escáner de material sensible
//!
//! El catálogo del pin contiene prosa (`"basic instructions"`) y el nombre
//! `session-id` aparece en los *bytes vigilados* de la fuente, no en el
//! perfil. El escáner del base es sensible a **valores**, no a nombres: una
//! palabra como `basic` sin un token con forma de credencial detrás no es un
//! secreto, y `session-id` como nombre de header tampoco. Este módulo no
//! sortea ni silencia el escáner: el perfil no lleva nombres de header de
//! sesión/thread (viven en los guards), y el catálogo real lo pasa tal cual.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::codex::{CODEX_REGION_GUARDS, CODEX_RELEASE_TAG, CODEX_WORKSPACE_VERSION, CodexCatalog};
use crate::client_contract::refresh::is_json_body;
use crate::client_contract::{
    CacheEntry, Candidate, ClientIdentity, ContractDriver, ContractError, ContractId,
    ContractLimits, ContractTransport, EvidenceKind, FetchRequest, FetchResponse,
    IDENTITY_ORIGIN_OFFICIAL, SourceDescriptor, SourceRef, TransportCache, TransportCacheKey,
    hex_digest, parse_version, validate_catalog_body_size, validate_source_body_size,
};

/// Identidad de las reglas compiladas de este driver.
pub const CODEX_RUNTIME_RULE_SET_ID: &str = "codex-runtime-release-pinned";

/// Versión de las reglas compiladas. Sube cuando cambia el set de guards, la
/// forma del perfil o el flujo de pin.
pub const CODEX_RUNTIME_EXTRACTOR_VERSION: u32 = 1;

/// Repositorio oficial del pin.
pub const CODEX_REPOSITORY: &str = "https://github.com/openai/codex";

/// Etiqueta de release del pin, idéntica a [`super::codex::CODEX_RELEASE_TAG`].
pub const CODEX_PINNED_RELEASE_TAG: &str = CODEX_RELEASE_TAG;

/// URL de la release estable mutable.
pub const CODEX_RELEASES_LATEST_URL: &str =
    "https://api.github.com/repos/openai/codex/releases/latest";

/// Máximo de desreferenciaciones de tag encadenadas aceptadas.
///
/// `refs/tags/x` → tag object → (tag object)\* → commit. Con una basta para un
/// annotated tag normal; se acepta una anidación más como margen y nunca se
/// sigue una cadena más larga, que sería estructura inesperada.
pub const MAX_TAG_DEREFERENCES: usize = 2;

/// `Accept` de la API de GitHub.
const ACCEPT_GITHUB_JSON: &str = "application/vnd.github+json";
/// `Accept` de los raw de GitHub.
const ACCEPT_RAW: &str = "application/octet-stream";
/// Scope de auth (etiqueta, nunca credencial) de las fuentes Codex.
const AUTH_SCOPE_GITHUB: &str = "github-public";

/// Perfil Codex: lo que el integrador necesita, y nada más.
///
/// Deliberadamente pequeño y sin nombres de header de sesión/thread: esos
/// nombres son *compiled rules* verificadas por guards sobre los bytes del pin
/// (y `session-id` es marcador de secreto en el escáner del base). El perfil
/// declara **qué** se verificó y contra qué revisión.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexProfile {
    /// Versión del cliente (`0.162.1`), igual a la del workspace del pin.
    pub version: String,
    /// Commit completo del pin (40 hex).
    pub commit: String,
    /// Tag anotado del que se partió.
    pub release_tag: String,
    /// Originator por defecto del cliente Codex.
    pub originator: String,
    /// Base URL del provider ChatGPT Codex.
    pub base_url: String,
    /// Nombre del header de residency.
    pub residency_header: String,
    /// Nombre del header de routing hint.
    pub routing_hint_header: String,
    /// Ruta del endpoint de modelos.
    pub models_endpoint_path: String,
    /// Parámetro de query que lleva la versión de cliente.
    pub client_version_query: String,
    /// Literales comprobados en la ruta de construcción del body.
    pub defaults: CodexRequestDefaults,
    /// Identidad de las reglas compiladas verificadas sobre estos bytes.
    pub rule_set_id: String,
    /// Versión de las reglas compiladas.
    pub extractor_version: u32,
    /// Número de slugs del catálogo del pin. El catálogo va aparte.
    pub catalog_models: usize,
}

/// Literales verificados en `core/src/client.rs` del pin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexRequestDefaults {
    /// `tool_choice` fijado al construir `ResponsesApiRequest`.
    pub tool_choice: String,
    /// `store` desactivado por defecto.
    pub store: bool,
    /// `stream` activado por defecto.
    pub stream: bool,
}

/// El pin resuelto: commit + versión, sin combinar `latest` con `main`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedPin {
    /// Tag anotado del pin.
    pub release_tag: String,
    /// Versión anunciada por la release, sin prefijo `v`.
    pub release_version: String,
    /// Commit completo (40 hex) al que apunta el tag object.
    pub commit: String,
}

impl ResolvedPin {
    /// La versión que el pin debe declarar en `[workspace.package]`.
    pub const fn expected_workspace_version(&self) -> &'static str {
        CODEX_WORKSPACE_VERSION
    }
}

/// Unidad adquirida en un salto: bytes, digest, revisión y evidencia.
///
/// La frescura no vive aquí: la lleva el adquisidor, porque un salto puede
/// devolver bytes cacheados (no frescos) y eso es una propiedad de la ronda,
/// no de la unidad.
#[derive(Clone, Debug)]
struct Acquired {
    url: String,
    body: Vec<u8>,
    digest: String,
    revision: Option<String>,
    evidence: EvidenceKind,
}

impl Acquired {
    /// La entrada de procedencia de esta unidad.
    fn source_ref(&self) -> SourceRef {
        SourceRef {
            url: self.url.clone(),
            revision: self.revision.clone(),
            hash: self.digest.clone(),
            evidence: self.evidence,
        }
    }
}

/// Constructor de un paso, reutilizando el [`SourceDescriptor`] del base.
fn step(
    url: String,
    accept: &str,
    evidence: EvidenceKind,
    revision: Option<String>,
) -> SourceDescriptor {
    SourceDescriptor {
        url,
        accept: accept.to_string(),
        auth_scope: AUTH_SCOPE_GITHUB.to_string(),
        headers: Vec::new(),
        evidence,
        revision,
    }
}

/// Adquisición de un recurso sobre el transporte compartido.
///
/// Reutiliza la cache, los límites y la semántica condicional del base: una
/// petición por recurso, `304` servido desde los bytes cacheados, `404`/`410`/
/// `429`/`5xx` mapeados a los errores del base. No hay un segundo GET de
/// revalidación ni un fetch duplicado del mismo recurso.
pub struct CodexReleaseAcquirer {
    transport: Arc<dyn ContractTransport>,
    cache: Arc<TransportCache>,
    limits: ContractLimits,
    fresh: AtomicUsize,
}

impl CodexReleaseAcquirer {
    /// Construye un adquisidor sobre infraestructura compartida.
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

    /// La cache compartida, para inspección o precalentamiento.
    pub fn cache(&self) -> &Arc<TransportCache> {
        &self.cache
    }

    /// Cuántos recursos devolvieron bytes frescos en esta ronda.
    pub fn fresh_count(&self) -> usize {
        self.fresh.load(Ordering::Relaxed)
    }

    /// Adquiere un recurso siguiendo la semántica condicional del base.
    ///
    /// Devuelve siempre bytes utilizables (frescos o cacheados); `Acquired::fresh`
    /// distingue el caso. Un `304` sin bytes cacheados es error real, y un
    /// cuerpo byte-idéntico al cacheado no cuenta como nuevo.
    async fn acquire(&self, descriptor: &SourceDescriptor) -> Result<Acquired, ContractError> {
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
            // La unidad se construye antes de mover la entrada al `touch`.
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
            // La unidad se construye antes de mover la entrada al `touch`.
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

/// La unidad que corresponde a bytes ya cacheados.
fn cached_unit(descriptor: &SourceDescriptor, entry: &CacheEntry) -> Acquired {
    let body = entry.body.clone().unwrap_or_default();
    Acquired {
        url: descriptor.url.clone(),
        body,
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

/// Orquestación async del pin completo de Codex.
///
/// Encadena los saltos y construye el [`Candidate`] final. No duplica refresh
/// ni validación: cada salto pasa por [`CodexReleaseAcquirer`] y la forma, los
/// límites y los secretos los verifica el base al commitear.
pub struct CodexReleaseDriver {
    transport: Arc<dyn ContractTransport>,
    cache: Arc<TransportCache>,
    limits: ContractLimits,
}

impl CodexReleaseDriver {
    /// Construye el driver sobre infraestructura compartida.
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
    pub fn cache(&self) -> &Arc<TransportCache> {
        &self.cache
    }

    /// Un adquisidor nuevo sobre la misma infraestructura.
    fn acquirer(&self) -> CodexReleaseAcquirer {
        CodexReleaseAcquirer::new(
            Arc::clone(&self.transport),
            Arc::clone(&self.cache),
            self.limits,
        )
    }

    /// Resuelve el pin partiendo de la release estable mutable.
    ///
    /// El tag anunciado por `releases/latest` **debe** ser
    /// [`CODEX_PINNED_RELEASE_TAG`]: si la release estable ya avanzó, no hay
    /// guards compilados para la nueva revisión y el candidato se rechaza en
    /// lugar de adoptarse.
    pub async fn resolve_pin(&self) -> Result<ResolvedPin, ContractError> {
        self.resolve_pin_with(&self.acquirer()).await
    }

    /// Resuelve el pin del tag ya fijado, sin consultar `releases/latest`.
    ///
    /// Es el camino que re-verifica la revisión compilada aunque la release
    /// estable haya avanzado.
    pub async fn resolve_pinned_tag(&self) -> Result<ResolvedPin, ContractError> {
        let (pin, _) = self
            .resolve_tag(
                CODEX_PINNED_RELEASE_TAG.to_string(),
                CODEX_WORKSPACE_VERSION.to_string(),
                &self.acquirer(),
            )
            .await?;
        Ok(pin)
    }

    /// Resuelve el pin reutilizando un adquisidor concreto.
    async fn resolve_pin_with(
        &self,
        acquirer: &CodexReleaseAcquirer,
    ) -> Result<ResolvedPin, ContractError> {
        let (pin, _) = self.resolve_pin_with_ref(acquirer).await?;
        Ok(pin)
    }

    /// Resuelve el pin y devuelve además la unidad del ref del tag.
    ///
    /// El ref es a la vez prueba de procedencia del pin y fuente del candidato,
    /// así que se adquiere **una sola vez** por ronda y se reenvía a
    /// [`Self::build_candidate_with_ref`] en lugar de volver a pedirlo.
    async fn resolve_pin_with_ref(
        &self,
        acquirer: &CodexReleaseAcquirer,
    ) -> Result<(ResolvedPin, Acquired), ContractError> {
        let latest = acquirer
            .acquire(&step(
                CODEX_RELEASES_LATEST_URL.to_string(),
                ACCEPT_GITHUB_JSON,
                EvidenceKind::ReleaseMetadata,
                None,
            ))
            .await?;
        let release = parse_release_metadata(&latest.body)?;
        if release.tag != CODEX_PINNED_RELEASE_TAG {
            return Err(ContractError::Rejected(format!(
                "la release estable es {:?} y el pin compilado es {:?}: sin guards para la nueva revisión",
                release.tag, CODEX_PINNED_RELEASE_TAG
            )));
        }
        if release.prerelease || release.draft {
            return Err(ContractError::Rejected(
                "la release estable del pin está marcada prerelease o draft".to_string(),
            ));
        }
        self.resolve_tag(release.tag, release.version, acquirer)
            .await
    }

    /// Resuelve `tag` → annotated tag → commit, con desreferenciación acotada.
    ///
    /// Devuelve también la unidad del ref: es la prueba de procedencia que el
    /// candidato reutiliza sin una segunda petición.
    async fn resolve_tag(
        &self,
        tag: String,
        version: String,
        acquirer: &CodexReleaseAcquirer,
    ) -> Result<(ResolvedPin, Acquired), ContractError> {
        let ref_url = format!("https://api.github.com/repos/openai/codex/git/refs/tags/{tag}");
        let reference_unit = acquirer
            .acquire(&step(
                ref_url,
                ACCEPT_GITHUB_JSON,
                EvidenceKind::ReleaseMetadata,
                Some(tag.clone()),
            ))
            .await?;
        let reference = parse_git_object(&reference_unit.body, "git/refs")?;
        if reference.object_type != "tag" {
            return Err(ContractError::Rejected(format!(
                "refs/tags/{tag} apunta a un objeto {:?}, no a un tag anotado",
                reference.object_type
            )));
        }
        if !is_commit_sha(&reference.sha) {
            return Err(ContractError::Rejected(format!(
                "el objeto tag {:?} no es un sha completo",
                reference.sha
            )));
        }

        let mut object_sha = reference.sha;
        let mut dereferences = 0usize;
        let commit = loop {
            let object_url =
                format!("https://api.github.com/repos/openai/codex/git/tags/{object_sha}");
            let object_unit = acquirer
                .acquire(&step(
                    object_url,
                    ACCEPT_GITHUB_JSON,
                    EvidenceKind::ReleaseMetadata,
                    Some(object_sha.clone()),
                ))
                .await?;
            let object = parse_git_object(&object_unit.body, "git/tags")?;
            if object.object_type == "commit" {
                if !is_commit_sha(&object.sha) {
                    return Err(ContractError::Rejected(format!(
                        "el commit resuelto {:?} no es un sha completo",
                        object.sha
                    )));
                }
                break object.sha;
            }
            if object.object_type != "tag" {
                return Err(ContractError::Rejected(format!(
                    "git/tags/{object_sha} apunta a un objeto {:?} inesperado",
                    object.object_type
                )));
            }
            dereferences += 1;
            if dereferences >= MAX_TAG_DEREFERENCES {
                return Err(ContractError::Rejected(format!(
                    "cadena de tags anidados más larga que {MAX_TAG_DEREFERENCES} desreferencias"
                )));
            }
            object_sha = object.sha;
        };

        Ok((
            ResolvedPin {
                release_tag: tag,
                release_version: version,
                commit,
            },
            reference_unit,
        ))
    }

    /// Construye el candidato completo del pin.
    ///
    /// Verifica, sobre los bytes del **mismo commit**: versión del workspace,
    /// guards de región, catálogo tipado y límites. El catálogo viaja como
    /// `catalog_bytes` separado del perfil pequeño.
    ///
    /// Adquiere el ref del tag (prueba de procedencia del pin) y reutiliza el
    /// resto de la ronda. [`Self::refresh`] usa la variante interna que no
    /// repite esa petición.
    pub async fn build_candidate(
        &self,
        pin: &ResolvedPin,
    ) -> Result<Candidate<CodexProfile>, ContractError> {
        let acquirer = self.acquirer();
        let ref_url = format!(
            "https://api.github.com/repos/openai/codex/git/refs/tags/{}",
            pin.release_tag
        );
        let ref_unit = acquirer
            .acquire(&step(
                ref_url,
                ACCEPT_GITHUB_JSON,
                EvidenceKind::ReleaseMetadata,
                Some(pin.release_tag.clone()),
            ))
            .await?;
        self.build_candidate_with_ref(pin, &ref_unit, &acquirer)
            .await
    }

    /// Construye el candidato reutilizando un adquisidor concreto y el ref ya
    /// adquirido en la resolución del pin (sin segunda petición del ref).
    async fn build_candidate_with_ref(
        &self,
        pin: &ResolvedPin,
        ref_unit: &Acquired,
        acquirer: &CodexReleaseAcquirer,
    ) -> Result<Candidate<CodexProfile>, ContractError> {
        // Fuente de procedencia del pin: el ref del tag anotado, ya adquirido.
        let mut sources = vec![ref_unit.source_ref()];

        // Cargo.toml del commit: la versión del workspace debe coincidir.
        let cargo_url = raw_url(&pin.commit, "codex-rs/Cargo.toml");
        let cargo = self
            .acquire_raw(
                acquirer,
                pin,
                "codex-rs/Cargo.toml",
                EvidenceKind::PackageManifest,
            )
            .await?;
        let cargo_text = decode_utf8(&cargo.body, &cargo_url)?;
        let workspace_version = workspace_package_version(&cargo_text).ok_or_else(|| {
            ContractError::Rejected(
                "codex-rs/Cargo.toml no declara `[workspace.package] version`".to_string(),
            )
        })?;
        let expected = pin.expected_workspace_version();
        if workspace_version != expected {
            // La versión que anuncia la release y la del workspace del pin no
            // coinciden: la revisión no es la esperada. `VersionConflict` del
            // base compara *generations* (digests), no versiones, así que aquí
            // corresponde un rechazo explícito.
            return Err(ContractError::Rejected(format!(
                "la versión del workspace del pin ({workspace_version}) no coincide \
                 con la anunciada por la release ({expected})"
            )));
        }
        parse_version(&workspace_version)?;
        sources.push(cargo.source_ref());

        // Fuentes vigiladas del mismo commit, verificadas por guards.
        let mut guards: Vec<(String, String)> = Vec::new();
        for guard in CODEX_REGION_GUARDS {
            if guards.iter().any(|(path, _)| path == guard.path) {
                continue;
            }
            let relative = format!("codex-rs/{}", guard.path);
            let unit = self
                .acquire_raw(acquirer, pin, &relative, EvidenceKind::BundleHash)
                .await?;
            let text = decode_utf8(&unit.body, &unit.url)?;
            sources.push(unit.source_ref());
            guards.push((guard.path.to_string(), text));
        }
        let guard_refs: Vec<(&'static str, &str)> = CODEX_REGION_GUARDS
            .iter()
            .map(|guard| {
                let text = guards
                    .iter()
                    .find(|(path, _)| path == guard.path)
                    .map_or("", |(_, text)| text.as_str());
                (guard.path, text)
            })
            .collect();
        // Reutiliza el verificador del parser: ni una reimplementación de
        // "el símbolo aparece exactamente una vez".
        super::codex::verify_region_guards(&guard_refs).map_err(|error| {
            ContractError::Rejected(format!("guards de región del pin: {error}"))
        })?;

        // Catálogo del mismo commit, tipado y aparte del perfil.
        let catalog = self
            .acquire_raw(
                acquirer,
                pin,
                "codex-rs/models-manager/models.json",
                EvidenceKind::RuntimeCatalog,
            )
            .await?;
        validate_catalog_body_size(catalog.body.len(), &self.limits)?;
        if !is_json_body(&catalog.body) {
            return Err(ContractError::Rejected(
                "models.json del pin no es JSON".to_string(),
            ));
        }
        let catalog_text = decode_utf8(&catalog.body, &catalog.url)?;
        // Un catálogo corrupto o con un enum no soportado es rechazo, nunca LKG
        // silencioso.
        let parsed = CodexCatalog::parse(&catalog_text)
            .map_err(|error| ContractError::Rejected(format!("catálogo del pin: {error}")))?;
        if parsed.models.is_empty() {
            return Err(ContractError::Rejected(
                "el catálogo del pin no declara modelos".to_string(),
            ));
        }
        if parsed.models.len() > self.limits.catalog_models {
            return Err(ContractError::CatalogTooLarge {
                models: parsed.models.len(),
                limit: self.limits.catalog_models,
            });
        }
        let profile = build_profile(pin, parsed.models.len());
        let identity = ClientIdentity::versioned(profile.version.clone(), IDENTITY_ORIGIN_OFFICIAL);
        sources.push(catalog.source_ref());

        Ok(Candidate::without_catalog(
            ContractId::Codex,
            identity,
            CODEX_RUNTIME_RULE_SET_ID,
            CODEX_RUNTIME_EXTRACTOR_VERSION,
            sources,
            profile,
        )
        .with_catalog(catalog.body, "json"))
    }

    /// Adquiere un raw del commit del pin.
    async fn acquire_raw(
        &self,
        acquirer: &CodexReleaseAcquirer,
        pin: &ResolvedPin,
        relative: &str,
        evidence: EvidenceKind,
    ) -> Result<Acquired, ContractError> {
        let url = raw_url(&pin.commit, relative);
        acquirer
            .acquire(&step(url, ACCEPT_RAW, evidence, Some(pin.commit.clone())))
            .await
    }

    /// Una ronda completa: resuelve el pin y construye el candidato.
    ///
    /// Devuelve `Ok(None)` cuando **ningún** recurso del pin trajo bytes
    /// frescos: la generación activa sigue en pie y no hay nada que commitear.
    pub async fn refresh(&self) -> Result<Option<Candidate<CodexProfile>>, ContractError> {
        let acquirer = self.acquirer();
        let (pin, ref_unit) = self.resolve_pin_with_ref(&acquirer).await?;
        let candidate = self
            .build_candidate_with_ref(&pin, &ref_unit, &acquirer)
            .await?;
        if acquirer.fresh_count() == 0 {
            return Ok(None);
        }
        Ok(Some(candidate))
    }
}

/// Construye el perfil a partir del pin resuelto y del catálogo verificado.
fn build_profile(pin: &ResolvedPin, catalog_models: usize) -> CodexProfile {
    CodexProfile {
        version: pin.release_version.clone(),
        commit: pin.commit.clone(),
        release_tag: pin.release_tag.clone(),
        originator: "codex_cli_rs".to_string(),
        base_url: "https://chatgpt.com/backend-api/codex".to_string(),
        residency_header: "x-openai-internal-codex-residency".to_string(),
        routing_hint_header: "x-codex-routing-hint".to_string(),
        models_endpoint_path: "models".to_string(),
        client_version_query: "client_version".to_string(),
        defaults: CodexRequestDefaults {
            tool_choice: "auto".to_string(),
            store: false,
            stream: true,
        },
        rule_set_id: CODEX_RUNTIME_RULE_SET_ID.to_string(),
        extractor_version: CODEX_RUNTIME_EXTRACTOR_VERSION,
        catalog_models,
    }
}

/// URL raw de un path del repo en un commit concreto.
fn raw_url(commit: &str, path: &str) -> String {
    format!("https://raw.githubusercontent.com/openai/codex/{commit}/{path}")
}

/// Decodifica UTF-8 o rechaza: un guard sobre bytes no textuales no tiene
/// sentido, y `from_utf8_lossy` silenciaría la corrupción.
fn decode_utf8(body: &[u8], url: &str) -> Result<String, ContractError> {
    std::str::from_utf8(body)
        .map(str::to_string)
        .map_err(|_| ContractError::Rejected(format!("{url} no es UTF-8 válido")))
}

/// Un sha de commit completo: 40 hex minúsculas.
fn is_commit_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Versión de `[workspace.package]` dentro de un `Cargo.toml`.
///
/// Lectura mínima de la tabla concreta, sin un parser TOML completo: se entra
/// en `[workspace.package]` y se lee la primera `version = "..."`.
fn workspace_package_version(cargo: &str) -> Option<String> {
    let mut in_section = false;
    for line in cargo.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = trimmed == "[workspace.package]";
            continue;
        }
        if !in_section {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("version") else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let value = rest.trim().trim_matches('"').trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

/// Metadata mutable de `releases/latest`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseMetadata {
    /// Tag de la release.
    pub tag: String,
    /// Versión anunciada, sin prefijo `v`.
    pub version: String,
    /// Marca de prerelease.
    pub prerelease: bool,
    /// Marca de draft.
    pub draft: bool,
}

/// Parsea `releases/latest`. Un campo de canal ausente se trata como
/// prerelease/draft: fail-closed, nunca se adopta por omisión.
pub fn parse_release_metadata(body: &[u8]) -> Result<ReleaseMetadata, ContractError> {
    let value = parse_json_object(body, "releases/latest")?;
    let tag = string_field(&value, "tag_name", "releases/latest")?;
    let version = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(&tag)
        .trim_start_matches('v')
        .to_string();
    Ok(ReleaseMetadata {
        tag,
        version,
        prerelease: value
            .get("prerelease")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        draft: value.get("draft").and_then(Value::as_bool).unwrap_or(true),
    })
}

/// Un objeto git (`refs` o `tags`) resuelto.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitObject {
    /// Sha del objeto apuntado.
    pub sha: String,
    /// Tipo del objeto apuntado (`tag`, `commit`, …).
    pub object_type: String,
}

/// Parsea `git/refs/...` o `git/tags/...`.
pub fn parse_git_object(body: &[u8], label: &str) -> Result<GitObject, ContractError> {
    let value = parse_json_object(body, label)?;
    let object = value
        .get("object")
        .ok_or_else(|| ContractError::Rejected(format!("{label} sin campo `object`")))?;
    Ok(GitObject {
        sha: string_field(object, "sha", label)?,
        object_type: string_field(object, "type", label)?,
    })
}

/// Objeto JSON o rechazo tipado.
fn parse_json_object(body: &[u8], label: &str) -> Result<Value, ContractError> {
    if !is_json_body(body) {
        return Err(ContractError::Rejected(format!("{label} no devolvió JSON")));
    }
    serde_json::from_slice(body).map_err(|error| ContractError::Encoding(Arc::new(error)))
}

/// Campo de texto obligatorio o rechazo tipado.
fn string_field(value: &Value, field: &str, label: &str) -> Result<String, ContractError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ContractError::Rejected(format!("{label} sin campo `{field}`")))
}

/// El último salto del pin, expresado como [`ContractDriver`] del base.
///
/// El `ContractRefresher` del base es dueño de la petición condicional, de la
/// cache y del parse cuando el caller ya resolvió el commit; este driver solo
/// aporta el descriptor del raw y su lectura.
pub struct CodexPinnedRawDriver {
    commit: String,
    path: String,
}

impl CodexPinnedRawDriver {
    /// Raw de `path` en el commit del pin.
    pub fn new(commit: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            commit: commit.into(),
            path: path.into(),
        }
    }

    /// El descriptor de la fuente, reutilizando el tipo del base.
    pub fn descriptor(&self) -> SourceDescriptor {
        step(
            raw_url(&self.commit, &self.path),
            ACCEPT_RAW,
            EvidenceKind::BundleHash,
            Some(self.commit.clone()),
        )
    }
}

/// `parse` no vuelve a pedir nada: consume los bytes que el
/// [`crate::client_contract::ContractRefresher`] ya adquirió y validó.
impl ContractDriver<String> for CodexPinnedRawDriver {
    fn contract_id(&self) -> ContractId {
        ContractId::Codex
    }

    fn rule_set_id(&self) -> &'static str {
        CODEX_RUNTIME_RULE_SET_ID
    }

    fn extractor_version(&self) -> u32 {
        CODEX_RUNTIME_EXTRACTOR_VERSION
    }

    fn source(&self) -> SourceDescriptor {
        self.descriptor()
    }

    fn parse(
        &self,
        body: &[u8],
        revision: Option<&str>,
        body_digest: &str,
    ) -> Result<Candidate<String>, ContractError> {
        let text = decode_utf8(body, &self.path)?;
        Ok(Candidate::without_catalog(
            ContractId::Codex,
            ClientIdentity::versioned(CODEX_WORKSPACE_VERSION, IDENTITY_ORIGIN_OFFICIAL),
            CODEX_RUNTIME_RULE_SET_ID,
            CODEX_RUNTIME_EXTRACTOR_VERSION,
            vec![SourceRef::pinned(
                raw_url(&self.commit, &self.path),
                revision.unwrap_or(&self.commit),
                body_digest,
                EvidenceKind::BundleHash,
            )],
            text,
        ))
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

    /// Commit del pin: `092d3acd6bec3e3a14bdc7e7a2810ab628ab759d`, el que el
    /// annotated tag `rust-v0.162.1` desreferencia (visible en
    /// `fixtures/codex-tag-object.json`). Un sha completo de 40 hex, que es la
    /// única forma que el driver acepta como revisión fijada.
    const PIN_COMMIT: &str = "092d3acd6bec3e3a14bdc7e7a2810ab628ab759d";

    fn ok_json(body: &str) -> FetchResponse {
        FetchResponse {
            status: 200,
            headers: vec![("etag".to_string(), "\"v1\"".to_string())],
            body: body.as_bytes().to_vec(),
        }
    }

    fn ok_raw(body: &[u8]) -> FetchResponse {
        FetchResponse {
            status: 200,
            headers: vec![("etag".to_string(), "\"v1\"".to_string())],
            body: body.to_vec(),
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

    fn driver(transport: MapTransport) -> CodexReleaseDriver {
        CodexReleaseDriver::new(
            Arc::new(transport),
            Arc::new(TransportCache::new()),
            limits(),
        )
    }

    /// Respuestas de un pin completo, con overrides por URL relativa.
    fn full_map(commit: &str, overrides: Vec<(&str, FetchResponse)>) -> MapTransport {
        let mut entries: Vec<(String, FetchResponse)> = vec![
            (
                CODEX_RELEASES_LATEST_URL.to_string(),
                ok_json(
                    r#"{"tag_name":"rust-v0.162.1","name":"0.162.1","prerelease":false,"draft":false}"#,
                ),
            ),
            (
                "https://api.github.com/repos/openai/codex/git/refs/tags/rust-v0.162.1".to_string(),
                ok_json(r#"{"object":{"sha":"7a078f6bb4ce0dc2a81408b334ce52979d92c603","type":"tag"}}"#),
            ),
            (
                "https://api.github.com/repos/openai/codex/git/tags/7a078f6bb4ce0dc2a81408b334ce52979d92c603".to_string(),
                ok_json(&format!(
                    r#"{{"object":{{"sha":"{commit}","type":"commit"}}}}"#
                )),
            ),
            (
                format!("https://raw.githubusercontent.com/openai/codex/{commit}/codex-rs/Cargo.toml"),
                ok_raw(&fixture_bytes("codex-rust-v0.162.1/codex-rs/Cargo.toml")),
            ),
            (
                format!(
                    "https://raw.githubusercontent.com/openai/codex/{commit}/codex-rs/models-manager/models.json"
                ),
                ok_raw(&fixture_bytes(
                    "codex-rust-v0.162.1/codex-rs/models-manager/models.json",
                )),
            ),
        ];
        for path in [
            "core/src/client.rs",
            "login/src/auth/default_client.rs",
            "model-provider-info/src/lib.rs",
            "codex-api/src/requests/headers.rs",
            "codex-api/src/endpoint/models.rs",
            "models-manager/src/lib.rs",
        ] {
            entries.push((
                format!("https://raw.githubusercontent.com/openai/codex/{commit}/codex-rs/{path}"),
                ok_raw(&fixture_bytes(&format!(
                    "codex-rust-v0.162.1/codex-rs/{path}"
                ))),
            ));
        }
        let map = MapTransport::with(entries);
        for (relative, response) in overrides {
            let url = format!(
                "https://raw.githubusercontent.com/openai/codex/{commit}/codex-rs/{relative}"
            );
            map.responses.lock().unwrap().insert(url, response);
        }
        map
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn pin() -> ResolvedPin {
        ResolvedPin {
            release_tag: "rust-v0.162.1".to_string(),
            release_version: "0.162.1".to_string(),
            commit: PIN_COMMIT.to_string(),
        }
    }

    // ----------
    // Release metadata
    // ----------

    #[test]
    fn release_metadata_reads_tag_version_and_channel() {
        let body =
            br#"{"tag_name":"rust-v0.162.1","name":"0.162.1","prerelease":false,"draft":false}"#;
        let meta = parse_release_metadata(body).unwrap();
        assert_eq!(meta.tag, "rust-v0.162.1");
        assert_eq!(meta.version, "0.162.1");
        assert!(!meta.prerelease);
        assert!(!meta.draft);
    }

    #[test]
    fn release_metadata_is_fail_closed_without_channel_fields() {
        let meta =
            parse_release_metadata(br#"{"tag_name":"rust-v0.162.1","name":"0.162.1"}"#).unwrap();
        assert!(meta.prerelease, "un campo ausente se trata como prerelease");
        assert!(meta.draft, "un campo ausente se trata como draft");
    }

    #[test]
    fn release_metadata_rejects_html_and_missing_tag() {
        assert!(parse_release_metadata(b"<html>nope</html>").is_err());
        assert!(parse_release_metadata(br#"{"name":"0.162.1"}"#).is_err());
    }

    #[tokio::test]
    async fn a_stable_release_outside_the_pin_is_rejected() {
        let transport = MapTransport::with(vec![(
            CODEX_RELEASES_LATEST_URL.to_string(),
            ok_json(
                r#"{"tag_name":"rust-v9.9.9","name":"9.9.9","prerelease":false,"draft":false}"#,
            ),
        )]);
        let error = driver(transport).resolve_pin().await.unwrap_err();
        assert!(
            matches!(error, ContractError::Rejected(ref message) if message.contains("sin guards")),
            "got {error:?}"
        );
    }

    // ----------
    // Resolución del tag
    // ----------

    #[tokio::test]
    async fn an_annotated_tag_resolves_to_its_commit() {
        let outcome = driver(full_map(PIN_COMMIT, Vec::new()))
            .resolve_pin()
            .await
            .unwrap();
        assert_eq!(outcome.commit, PIN_COMMIT);
        assert_eq!(outcome.release_tag, "rust-v0.162.1");
        assert_eq!(outcome.expected_workspace_version(), "0.162.1");
    }

    #[tokio::test]
    async fn a_lightweight_tag_pointing_at_a_commit_is_rejected() {
        let transport = MapTransport::with(vec![
            (
                CODEX_RELEASES_LATEST_URL.to_string(),
                ok_json(
                    r#"{"tag_name":"rust-v0.162.1","name":"0.162.1","prerelease":false,"draft":false}"#,
                ),
            ),
            (
                "https://api.github.com/repos/openai/codex/git/refs/tags/rust-v0.162.1".to_string(),
                ok_json(&format!(
                    r#"{{"object":{{"sha":"{PIN_COMMIT}","type":"commit"}}}}"#
                )),
            ),
        ]);
        let error = driver(transport).resolve_pin().await.unwrap_err();
        assert!(
            matches!(error, ContractError::Rejected(ref message) if message.contains("tag anotado")),
            "un tag ligero no fija la revisión como un annotated tag, got {error:?}"
        );
    }

    #[tokio::test]
    async fn a_nested_tag_chain_beyond_the_limit_is_rejected() {
        let chained =
            r#"{"object":{"sha":"2222222222222222222222222222222222222222","type":"tag"}}"#;
        let transport = MapTransport::with(vec![
            (
                CODEX_RELEASES_LATEST_URL.to_string(),
                ok_json(r#"{"tag_name":"rust-v0.162.1","name":"0.162.1","prerelease":false,"draft":false}"#),
            ),
            (
                "https://api.github.com/repos/openai/codex/git/refs/tags/rust-v0.162.1".to_string(),
                ok_json(r#"{"object":{"sha":"3333333333333333333333333333333333333333","type":"tag"}}"#),
            ),
            (
                "https://api.github.com/repos/openai/codex/git/tags/3333333333333333333333333333333333333333".to_string(),
                ok_json(chained),
            ),
            (
                "https://api.github.com/repos/openai/codex/git/tags/2222222222222222222222222222222222222222".to_string(),
                ok_json(chained),
            ),
        ]);
        let error = driver(transport).resolve_pin().await.unwrap_err();
        assert!(
            matches!(error, ContractError::Rejected(ref message) if message.contains("desreferencias")),
            "una cadena de tags anidados debe cortarse, got {error:?}"
        );
    }

    // ----------
    // Errores de transporte
    // ----------

    /// Un `304` sin bytes cacheados es un error real: el refresher del base no
    /// tiene nada que servir y el pin no puede resolverse.
    #[tokio::test]
    async fn a_not_modified_without_cached_bytes_is_rejected() {
        let transport = MapTransport::with(vec![(
            CODEX_RELEASES_LATEST_URL.to_string(),
            FetchResponse {
                status: 304,
                headers: vec![("etag".to_string(), "\"v1\"".to_string())],
                body: Vec::new(),
            },
        )]);
        let error = driver(transport).refresh().await.unwrap_err();
        assert!(
            matches!(error, ContractError::MissingCachedBytes { .. }),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn a_missing_source_is_a_source_missing_error() {
        let transport = MapTransport::with(vec![(
            CODEX_RELEASES_LATEST_URL.to_string(),
            FetchResponse {
                status: 404,
                headers: Vec::new(),
                body: Vec::new(),
            },
        )]);
        let error = driver(transport).refresh().await.unwrap_err();
        assert!(
            matches!(error, ContractError::SourceMissing { .. }),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn a_rate_limit_carries_retry_after() {
        let transport = MapTransport::with(vec![(
            CODEX_RELEASES_LATEST_URL.to_string(),
            FetchResponse {
                status: 429,
                headers: vec![("retry-after".to_string(), "120".to_string())],
                body: Vec::new(),
            },
        )]);
        let error = driver(transport).refresh().await.unwrap_err();
        assert!(
            matches!(
                error,
                ContractError::RateLimited {
                    retry_after: Some(120),
                    ..
                }
            ),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn a_server_error_is_a_source_unavailable_error() {
        let transport = MapTransport::with(vec![(
            CODEX_RELEASES_LATEST_URL.to_string(),
            FetchResponse {
                status: 503,
                headers: Vec::new(),
                body: Vec::new(),
            },
        )]);
        let error = driver(transport).refresh().await.unwrap_err();
        assert!(
            matches!(error, ContractError::SourceUnavailable { status: 503, .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn an_oversized_source_body_is_refused_before_parsing() {
        let transport = MapTransport::with(vec![(
            CODEX_RELEASES_LATEST_URL.to_string(),
            FetchResponse {
                status: 200,
                headers: Vec::new(),
                body: vec![b'x'; 4096],
            },
        )]);
        let tight = CodexReleaseDriver::new(
            Arc::new(transport),
            Arc::new(TransportCache::new()),
            ContractLimits {
                source_bytes: 64,
                ..ContractLimits::default()
            },
        );
        let error = runtime().block_on(tight.refresh()).unwrap_err();
        assert!(
            matches!(error, ContractError::SourceTooLarge { .. }),
            "got {error:?}"
        );
    }

    // ----------
    // Construcción del candidato
    // ----------

    #[tokio::test]
    async fn a_full_pin_builds_a_candidate_with_a_separate_catalog() {
        let catalog = fixture_bytes("codex-rust-v0.162.1/codex-rs/models-manager/models.json");
        let candidate = driver(full_map(PIN_COMMIT, Vec::new()))
            .build_candidate(&pin())
            .await
            .unwrap();

        assert_eq!(candidate.contract_id, ContractId::Codex);
        assert_eq!(candidate.payload.version, "0.162.1");
        assert_eq!(candidate.payload.commit, PIN_COMMIT);
        assert_eq!(candidate.payload.release_tag, "rust-v0.162.1");
        assert_eq!(candidate.payload.catalog_models, 11);
        assert_eq!(candidate.payload.defaults.tool_choice, "auto");
        assert!(!candidate.payload.defaults.store);
        assert!(candidate.payload.defaults.stream);
        assert_eq!(candidate.rule_set_id, CODEX_RUNTIME_RULE_SET_ID);
        assert_eq!(candidate.extractor_version, CODEX_RUNTIME_EXTRACTOR_VERSION);

        // Catálogo separado: digest correcto y cuerpo presente en memoria.
        assert!(candidate.catalog_backed());
        assert_eq!(candidate.catalog_body(), Some(catalog.as_slice()));
        assert_eq!(
            candidate.catalog_ref.as_ref().unwrap().digest,
            hex_digest(&catalog)
        );
        assert_eq!(candidate.catalog_ref.as_ref().unwrap().format, "json");

        // El envelope serializado no inlinea el catálogo ni bytes de catálogo.
        let encoded = serde_json::to_string(&candidate).unwrap();
        assert!(!encoded.contains("gpt-6-sol"), "el catálogo no se inlinea");
        assert!(!encoded.contains("catalog_bytes"));
        assert!(
            encoded.len() < 64 * 1024,
            "el perfil debe caber en el límite"
        );

        // Una fuente por archivo vigilado + Cargo.toml + ref del tag + catálogo.
        assert_eq!(candidate.sources.len(), 9);
        assert!(
            candidate
                .sources
                .iter()
                .all(|source| source.revision.as_deref() == Some(PIN_COMMIT)
                    || source.evidence == EvidenceKind::ReleaseMetadata)
        );
    }

    #[tokio::test]
    async fn a_workspace_version_that_disagrees_with_the_release_is_rejected() {
        let cargo = String::from_utf8(fixture_bytes("codex-rust-v0.162.1/codex-rs/Cargo.toml"))
            .unwrap()
            .replace("version = \"0.162.1\"", "version = \"0.999.9\"");
        let transport = full_map(PIN_COMMIT, vec![("Cargo.toml", ok_raw(cargo.as_bytes()))]);
        let error = driver(transport).build_candidate(&pin()).await.unwrap_err();
        assert!(
            matches!(
                error,
                ContractError::Rejected(ref message)
                    if message.contains("no coincide") && message.contains("0.999.9")
            ),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn a_cargo_toml_without_workspace_version_is_rejected() {
        let transport = full_map(
            PIN_COMMIT,
            vec![("Cargo.toml", ok_raw(b"[workspace]\nmembers = []\n"))],
        );
        let error = driver(transport).build_candidate(&pin()).await.unwrap_err();
        assert!(
            matches!(error, ContractError::Rejected(ref message) if message.contains("workspace.package")),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn a_changed_region_guard_rejects_the_candidate() {
        // `store: false,` desaparece: el pin ya no es la revisión compilada.
        let client = String::from_utf8(fixture_bytes(
            "codex-rust-v0.162.1/codex-rs/core/src/client.rs",
        ))
        .unwrap()
        .replace("store: false,", "store: true,");
        let transport = full_map(
            PIN_COMMIT,
            vec![("core/src/client.rs", ok_raw(client.as_bytes()))],
        );
        let error = driver(transport).build_candidate(&pin()).await.unwrap_err();
        assert!(
            matches!(error, ContractError::Rejected(ref message) if message.contains("guards de región")),
            "un guard ausente debe rechazar el candidato, got {error:?}"
        );
    }

    #[tokio::test]
    async fn an_unknown_catalog_enum_rejects_the_candidate() {
        let transport = full_map(
            PIN_COMMIT,
            vec![(
                "models-manager/models.json",
                ok_raw(br#"{"models":[{"slug":"x","visibility":"banana"}]}"#),
            )],
        );
        let error = driver(transport).build_candidate(&pin()).await.unwrap_err();
        assert!(
            matches!(error, ContractError::Rejected(ref message) if message.contains("catálogo")),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn a_non_json_catalog_rejects_the_candidate() {
        let transport = full_map(
            PIN_COMMIT,
            vec![("models-manager/models.json", ok_raw(b"<html>nope</html>"))],
        );
        let error = driver(transport).build_candidate(&pin()).await.unwrap_err();
        assert!(matches!(error, ContractError::Rejected(_)), "got {error:?}");
    }

    #[tokio::test]
    async fn a_missing_watched_file_rejects_the_candidate() {
        let transport = MapTransport::with(vec![
            (
                CODEX_RELEASES_LATEST_URL.to_string(),
                ok_json(r#"{"tag_name":"rust-v0.162.1","name":"0.162.1","prerelease":false,"draft":false}"#),
            ),
            (
                "https://api.github.com/repos/openai/codex/git/refs/tags/rust-v0.162.1".to_string(),
                ok_json(r#"{"object":{"sha":"7a078f6bb4ce0dc2a81408b334ce52979d92c603","type":"tag"}}"#),
            ),
            (
                "https://api.github.com/repos/openai/codex/git/tags/7a078f6bb4ce0dc2a81408b334ce52979d92c603".to_string(),
                ok_json(&format!(r#"{{"object":{{"sha":"{PIN_COMMIT}","type":"commit"}}}}"#)),
            ),
            (
                format!("https://raw.githubusercontent.com/openai/codex/{PIN_COMMIT}/codex-rs/Cargo.toml"),
                ok_raw(&fixture_bytes("codex-rust-v0.162.1/codex-rs/Cargo.toml")),
            ),
        ]);
        let error = driver(transport).build_candidate(&pin()).await.unwrap_err();
        assert!(
            matches!(error, ContractError::SourceMissing { .. }),
            "got {error:?}"
        );
    }

    // ----------
    // Ronda completa, cache y reuso
    // ----------

    #[tokio::test]
    async fn a_first_round_returns_a_candidate_and_warms_the_cache() {
        let cache = Arc::new(TransportCache::new());
        let driver = CodexReleaseDriver::new(
            Arc::new(full_map(PIN_COMMIT, Vec::new())),
            Arc::clone(&cache),
            limits(),
        );
        let candidate = driver.refresh().await.unwrap().expect("un candidato");
        assert_eq!(candidate.payload.commit, PIN_COMMIT);
        // Tres saltos API (latest, ref, tag object) + Cargo.toml + catálogo +
        // los 6 archivos vigilados.
        assert_eq!(cache.len(), 11, "una entrada por recurso adquirido");
    }

    #[tokio::test]
    async fn a_second_round_where_everything_is_not_modified_yields_nothing() {
        let cache = Arc::new(TransportCache::new());
        let first = CodexReleaseDriver::new(
            Arc::new(full_map(PIN_COMMIT, Vec::new())),
            Arc::clone(&cache),
            limits(),
        );
        assert!(first.refresh().await.unwrap().is_some());

        // Segunda ronda: todas las fuentes responden 304 con los bytes en cache.
        let not_modified: Vec<(String, FetchResponse)> = cache_urls(&cache)
            .into_iter()
            .map(|url| {
                (
                    url,
                    FetchResponse {
                        status: 304,
                        headers: vec![("etag".to_string(), "\"v1\"".to_string())],
                        body: Vec::new(),
                    },
                )
            })
            .collect();
        let second = CodexReleaseDriver::new(
            Arc::new(MapTransport::with(not_modified)),
            Arc::clone(&cache),
            limits(),
        );
        let outcome = second.refresh().await.unwrap();
        assert!(outcome.is_none(), "sin bytes frescos no hay candidato");
    }

    #[tokio::test]
    async fn a_second_round_sends_conditional_headers() {
        let cache = Arc::new(TransportCache::new());
        let first = CodexReleaseDriver::new(
            Arc::new(full_map(PIN_COMMIT, Vec::new())),
            Arc::clone(&cache),
            limits(),
        );
        assert!(first.refresh().await.unwrap().is_some());

        // Segunda ronda con un transporte vacío: la petición debe llevar los
        // validadores cacheados, o el upstream la trataría como incondicional
        // y devolvería 200 con los mismos bytes.
        let transport = Arc::new(MapTransport::with(Vec::new()));
        let seen = Arc::clone(&transport);
        let second = CodexReleaseDriver::new(transport, Arc::clone(&cache), limits());
        assert!(
            second.resolve_pin().await.is_err(),
            "el transporte vacío responde 404"
        );

        let requests = seen.requests();
        assert!(!requests.is_empty(), "al menos una petición salió");
        assert!(
            requests.iter().any(|request| request
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("if-none-match"))),
            "la ronda 2 debe reenviar If-None-Match: {:?}",
            requests
                .iter()
                .map(|request| request.headers.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(cache.len(), 11, "una entrada por recurso del pin");
    }

    #[tokio::test]
    async fn a_byte_identical_body_does_not_count_as_fresh() {
        let cache = Arc::new(TransportCache::new());
        let first = CodexReleaseDriver::new(
            Arc::new(full_map(PIN_COMMIT, Vec::new())),
            Arc::clone(&cache),
            limits(),
        );
        assert!(first.refresh().await.unwrap().is_some());

        // Segunda ronda: mismos bytes con un ETag nuevo.
        let second = CodexReleaseDriver::new(
            Arc::new(full_map(PIN_COMMIT, Vec::new())),
            Arc::clone(&cache),
            limits(),
        );
        let outcome = second.refresh().await.unwrap();
        assert!(
            outcome.is_none(),
            "bytes idénticos no deben producir una generación nueva"
        );
    }

    // ----------
    // Perfil y driver del último salto
    // ----------

    #[test]
    fn the_profile_is_small_and_carries_no_session_header() {
        let profile = build_profile(&pin(), 11);
        let encoded = serde_json::to_string(&profile).unwrap();
        assert!(
            encoded.len() < 1024,
            "el perfil mide {} bytes",
            encoded.len()
        );
        assert!(
            !crate::client_contract::validation::contains_secret_material(encoded.as_bytes()),
            "el perfil no puede contener material de credencial: {encoded}"
        );
        assert!(
            !encoded.contains("session-id"),
            "el detalle de wire vive en los guards"
        );
        assert!(!encoded.contains("thread-id"));
    }

    #[test]
    fn the_profile_survives_a_serde_roundtrip() {
        let profile = build_profile(&pin(), 11);
        let encoded = serde_json::to_vec(&profile).unwrap();
        let loaded: CodexProfile = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(loaded, profile);
    }

    #[test]
    fn the_pinned_raw_driver_consumes_bytes_without_refetching() {
        let raw = CodexPinnedRawDriver::new(PIN_COMMIT, "codex-rs/models-manager/models.json");
        let descriptor = raw.descriptor();
        assert_eq!(descriptor.revision.as_deref(), Some(PIN_COMMIT));
        assert!(descriptor.url.contains(PIN_COMMIT));
        assert_eq!(descriptor.evidence, EvidenceKind::BundleHash);
        assert_eq!(descriptor.auth_scope, "github-public");

        let body = br#"{"models":[]}"#;
        let candidate = raw
            .parse(body, Some(PIN_COMMIT), &hex_digest(body))
            .unwrap();
        assert_eq!(candidate.payload, "{\"models\":[]}");
        assert_eq!(candidate.sources.len(), 1);
        assert_eq!(candidate.sources[0].revision.as_deref(), Some(PIN_COMMIT));
        assert_eq!(candidate.rule_set_id, CODEX_RUNTIME_RULE_SET_ID);
        assert_eq!(
            candidate.client_identity.version.as_deref(),
            Some("0.162.1")
        );
    }

    #[test]
    fn a_non_utf8_raw_is_rejected() {
        let raw = CodexPinnedRawDriver::new(PIN_COMMIT, "codex-rs/core/src/client.rs");
        let error = raw
            .parse(
                &[0xff, 0xfe, 0x00],
                Some(PIN_COMMIT),
                &hex_digest(&[0xff, 0xfe, 0x00]),
            )
            .unwrap_err();
        assert!(matches!(error, ContractError::Rejected(_)), "got {error:?}");
    }

    #[test]
    fn workspace_package_version_reads_only_the_pinned_section() {
        let cargo = "[workspace]\nversion = \"9.9.9\"\n\n[workspace.package]\nversion = \"0.162.1\"\nedition = \"2024\"\n";
        assert_eq!(workspace_package_version(cargo).as_deref(), Some("0.162.1"));
        assert!(workspace_package_version("[workspace]\nversion = \"9.9.9\"\n").is_none());
        assert!(workspace_package_version("").is_none());
    }

    #[test]
    fn commit_shas_must_be_complete_lowercase_hex() {
        assert!(is_commit_sha(PIN_COMMIT));
        assert!(!is_commit_sha(&PIN_COMMIT.to_uppercase()));
        assert!(!is_commit_sha("5f2a1c0d"));
        assert!(!is_commit_sha(&format!("{PIN_COMMIT}z")));
    }

    /// Las URLs del pin, para preparar una ronda `304` representativa.
    ///
    /// Verifica que cada una esté ya cacheada: si alguna no lo está, la ronda
    /// 304 que prepara este helper no probaría lo que dice probar.
    fn cache_urls(cache: &TransportCache) -> Vec<String> {
        let mut urls = vec![
            CODEX_RELEASES_LATEST_URL.to_string(),
            "https://api.github.com/repos/openai/codex/git/refs/tags/rust-v0.162.1".to_string(),
            "https://api.github.com/repos/openai/codex/git/tags/7a078f6bb4ce0dc2a81408b334ce52979d92c603".to_string(),
            format!("https://raw.githubusercontent.com/openai/codex/{PIN_COMMIT}/codex-rs/Cargo.toml"),
            format!(
                "https://raw.githubusercontent.com/openai/codex/{PIN_COMMIT}/codex-rs/models-manager/models.json"
            ),
        ];
        for path in [
            "core/src/client.rs",
            "login/src/auth/default_client.rs",
            "model-provider-info/src/lib.rs",
            "codex-api/src/requests/headers.rs",
            "codex-api/src/endpoint/models.rs",
            "models-manager/src/lib.rs",
        ] {
            urls.push(format!(
                "https://raw.githubusercontent.com/openai/codex/{PIN_COMMIT}/codex-rs/{path}"
            ));
        }
        for url in &urls {
            let accept = if url.contains("api.github.com") {
                ACCEPT_GITHUB_JSON
            } else {
                ACCEPT_RAW
            };
            assert!(
                cache
                    .get(&TransportCacheKey::new(
                        url.as_str(),
                        accept,
                        AUTH_SCOPE_GITHUB
                    ))
                    .is_some(),
                "la cache debe tener {url}"
            );
        }
        urls
    }
}
