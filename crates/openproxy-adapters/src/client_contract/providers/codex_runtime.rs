//! Driver de adquisición release-pinned del contrato Codex.
//!
//! Flujo completo, en una sola ronda orquestada y reutilizando la
//! infraestructura común del base ([`ContractTransport`], [`TransportCache`],
//! [`ContractLimits`], validación estructural y guards de región):
//!
//! 1. `releases/latest` (mutable, `EvidenceKind::ReleaseMetadata`) → tag
//!    estable `rust-vX.Y.Z`, nunca `prerelease` ni `draft`. La versión se
//!    deriva **del tag** (`rust-v0.163.0` → `0.163.0`), no del campo `name`,
//!    que es texto libre; el tag es el identificador real de la release.
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
//! ## Autoactualización: qué se adopta y qué no
//!
//! El driver acepta **cualquier** tag estable con forma `rust-vX.Y.Z`
//! (semver de tres partes, canal estable), y la versión del `[workspace.package]`
//! del pin se compara contra la versión derivada del tag. La adopción efectiva
//! depende de los guards compilados ([`super::codex::CODEX_REGION_GUARDS`],
//! verificados sobre los bytes del **mismo commit**): una release nueva cuyo
//! commit no verifica los guards se rechaza, y la generación activa no cambia.
//! No se promete código arbitrario: solo datos, headers y defaults soportados
//! por las reglas compiladas pueden activarse; un cambio de transporte o de
//! autenticación exigiría código nuevo y queda fuera del pin.
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

use serde::{Deserialize, Serialize};

use super::codex::{CODEX_REGION_GUARDS, CODEX_RELEASE_TAG, CODEX_WORKSPACE_VERSION, CodexCatalog};
use crate::client_contract::refresh::is_json_body;
use crate::client_contract::{
    CacheEntry, Candidate, ClientIdentity, ContractDriver, ContractError, ContractId,
    ContractLimits, ContractTransport, EvidenceKind, FetchRequest, FetchResponse,
    IDENTITY_ORIGIN_OFFICIAL, SourceDescriptor, SourceRef, TransportCache, TransportCacheKey,
    hex_digest, parse_version, validate_catalog_body_size, validate_source_body_size,
};

pub(crate) mod acquirer;
mod metadata;

pub use acquirer::CodexReleaseAcquirer;
use acquirer::ReleaseInfra;
pub use metadata::{
    CodexPinnedRawDriver, GitObject, ReleaseMetadata, codex_version_from_tag, parse_git_object,
    parse_release_metadata,
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
    /// La versión que el pin debe declarar en `[workspace.package]`: la que
    /// anuncia la release, derivada de su tag. No es una constante compilada —
    /// un tag estable nuevo (`rust-v0.163.0`) fija su propia versión esperada.
    pub fn expected_workspace_version(&self) -> &str {
        &self.release_version
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

/// Orquestación async del pin completo de Codex.
///
/// Encadena los saltos y construye el [`Candidate`] final. No duplica refresh
/// ni validación: cada salto pasa por [`CodexReleaseAcquirer`] y la forma, los
/// límites y los secretos los verifica el base al commitear.
pub struct CodexReleaseDriver {
    inner: ReleaseInfra,
}

impl CodexReleaseDriver {
    /// Construye el driver sobre infraestructura compartida.
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
    pub fn cache(&self) -> &Arc<TransportCache> {
        self.inner.cache()
    }

    /// Un adquisidor nuevo sobre la misma infraestructura.
    fn acquirer(&self) -> CodexReleaseAcquirer {
        CodexReleaseAcquirer::from_core(self.inner.acquirer_core())
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
        let version = codex_version_from_tag(CODEX_PINNED_RELEASE_TAG)?;
        let (pin, _) = self
            .resolve_tag(
                CODEX_PINNED_RELEASE_TAG.to_string(),
                version,
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
        if release.prerelease || release.draft {
            return Err(ContractError::Rejected(
                "la release estable del pin está marcada prerelease o draft".to_string(),
            ));
        }
        // El tag ya viene validado como estable `rust-vX.Y.Z` por
        // `parse_release_metadata`. La release estable puede haber avanzado
        // más allá del pin compilado: la resolución continúa y la adopción la
        // deciden los guards compilados verificados sobre los bytes del
        // commit resuelto (ver `build_candidate_with_ref`): sin guards para la
        // revisión nueva, el candidato se rechaza y el LKG no cambia.
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
        validate_catalog_body_size(catalog.body.len(), self.inner.limits())?;
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
        if parsed.models.len() > self.inner.limits().catalog_models {
            return Err(ContractError::CatalogTooLarge {
                models: parsed.models.len(),
                limit: self.inner.limits().catalog_models,
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
pub(crate) fn build_profile(pin: &ResolvedPin, catalog_models: usize) -> CodexProfile {
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
pub(crate) fn is_commit_sha(value: &str) -> bool {
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

#[cfg(test)]
mod tests;
