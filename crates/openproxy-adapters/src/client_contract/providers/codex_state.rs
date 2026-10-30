//! Runtime de contrato Codex: store global, seed embebido y refresh validado.
//!
//! Une el driver release-pinned ([`super::codex_runtime`]) con el store del
//! base ([`crate::client_contract::store`]):
//!
//! * **Store global tipado.** [`runtime()`] expone un único
//!   [`CodexContractRuntime`] con el LKG en disco
//!   (`$OPENPROXY_CODEX_STORE_DIR` o `~/.openproxy/contracts`), accesible
//!   desde el server sin reconstruir nada por petición.
//! * **Bootstrap embebido.** [`embedded_seed`] construye un snapshot válido
//!   del pin actual (`rust-v0.162.1`) con los fixtures reales compilados en el
//!   binario ([`include_str!`]). Con disco vacío, [`CodexContractRuntime::load`]
//!   sirve el seed: el proxy arranca siempre con una generación activa.
//! * **Refresh validado.** [`CodexContractRuntime::refresh`] usa el driver y el
//!   validador [`CodexContractValidator`]; un candidato que no verifique los
//!   guards se rechaza y el snapshot activo no cambia.
//!
//! El runtime no duplica lógica del base: reutiliza `ContractStore` (LKG,
//! anti-downgrade, persistencia atómica), `TransportCache` (condicionales) y
//! `CodexReleaseDriver` (pin de red). Solo añade el pegamento específico de
//! Codex: procedencia real del pin, validación de reglas compiladas y el
//! punto de entrada global para el server.

use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use serde_json::Value;

use super::codex::{CODEX_REGION_GUARDS, CodexCatalog};
use super::codex_runtime::{
    CODEX_RUNTIME_EXTRACTOR_VERSION, CODEX_RUNTIME_RULE_SET_ID, CodexProfile, CodexReleaseDriver,
    ResolvedPin, build_profile, codex_version_from_tag,
};
use crate::client_contract::{
    Candidate, ClientIdentity, ContractError, ContractId, ContractLimits, ContractTransport,
    ContractValidator, EvidenceKind, IDENTITY_ORIGIN_OFFICIAL, Snapshot, SourceRef, TransportCache,
    VersionChannel, hex_digest, parse_version,
};

#[doc(hidden)]
pub mod shared;
use shared::{ActivatableVersion, ContractProvider, ProviderRuntime};
pub(crate) use shared::{check_compiled_rules, default_store_root_for};

const TAG_REF_JSON: &str = include_str!("../../../../../fixtures/codex-tag-ref.json");
const TAG_OBJECT_JSON: &str = include_str!("../../../../../fixtures/codex-tag-object.json");
const PINNED_CARGO_TOML: &str =
    include_str!("../../../../../fixtures/codex-rust-v0.162.1/codex-rs/Cargo.toml");
const PINNED_MODELS_JSON: &str =
    include_str!("../../../../../fixtures/codex-rust-v0.162.1/codex-rs/models-manager/models.json");

/// Archivos fuente de los guards compilados, en la misma forma que el driver
/// los lee (rutas relativas a `codex-rs/`).
const GUARD_FILES: &[(&str, &str)] = &[
    (
        "core/src/client.rs",
        include_str!("../../../../../fixtures/codex-rust-v0.162.1/codex-rs/core/src/client.rs"),
    ),
    (
        "login/src/auth/default_client.rs",
        include_str!(
            "../../../../../fixtures/codex-rust-v0.162.1/codex-rs/login/src/auth/default_client.rs"
        ),
    ),
    (
        "model-provider-info/src/lib.rs",
        include_str!(
            "../../../../../fixtures/codex-rust-v0.162.1/codex-rs/model-provider-info/src/lib.rs"
        ),
    ),
    (
        "codex-api/src/requests/headers.rs",
        include_str!(
            "../../../../../fixtures/codex-rust-v0.162.1/codex-rs/codex-api/src/requests/headers.rs"
        ),
    ),
    (
        "codex-api/src/endpoint/models.rs",
        include_str!(
            "../../../../../fixtures/codex-rust-v0.162.1/codex-rs/codex-api/src/endpoint/models.rs"
        ),
    ),
    (
        "models-manager/src/lib.rs",
        include_str!(
            "../../../../../fixtures/codex-rust-v0.162.1/codex-rs/models-manager/src/lib.rs"
        ),
    ),
];

/// Un campo del fixture del pin, o aborta: un fixture roto es un error de
/// build, no un estado degradado que el runtime deba manejar.
fn fixture_field(json: &str, name: &str, path: &[&str]) -> String {
    let value: Value = serde_json::from_slice(json.as_bytes())
        .unwrap_or_else(|error| panic!("{name} inválido: {error}"));
    let mut current = value
        .get(path[0])
        .unwrap_or_else(|| panic!("{name} no declara {}", path[0]));
    for key in &path[1..] {
        current = current
            .get(*key)
            .unwrap_or_else(|| panic!("{name} no declara {}.{}", path[0], key));
    }
    current.as_str().map_or_else(
        || panic!("{name}.{} no es texto", path.join(".")),
        str::to_string,
    )
}

/// Tag estable del pin leído del fixture del ref (`refs/tags/<tag>`).
fn pin_tag() -> String {
    fixture_field(TAG_REF_JSON, "codex-tag-ref.json", &["ref"])
        .rsplit('/')
        .next()
        .map_or_else(
            || panic!("codex-tag-ref.json no declara ref"),
            str::to_string,
        )
}

/// Commit del pin: el `object.sha` que el annotated tag desreferencia, la
/// única revisión fijada que el driver acepta.
fn pin_commit() -> String {
    fixture_field(TAG_OBJECT_JSON, "codex-tag-object.json", &["object", "sha"])
}

/// Sha del annotated tag object, que es a donde apunta el ref. Solo lo
/// usan los tests: la semilla embebida no necesita el sha del tag object.
#[cfg(test)]
fn pin_tag_object_sha() -> String {
    fixture_field(TAG_OBJECT_JSON, "codex-tag-object.json", &["sha"])
}

/// URL del ref del tag, tomada del fixture real.
fn tag_ref_url() -> String {
    fixture_field(TAG_REF_JSON, "codex-tag-ref.json", &["url"])
}

/// URL del annotated tag object, tomada del fixture real.
fn tag_object_url() -> String {
    fixture_field(TAG_OBJECT_JSON, "codex-tag-object.json", &["url"])
}

/// Prefijo de URLs raw del commit, en la misma forma que el driver.
fn raw_prefix(commit: &str) -> String {
    format!("https://raw.githubusercontent.com/openai/codex/{commit}/codex-rs")
}

/// Semilla embebida: un snapshot válido del pin actual construido desde los
/// fixtures reales, con procedencia por cada byte. Con disco vacío es la
/// primera generación activa.
fn embedded_seed() -> Snapshot<CodexProfile> {
    let tag = pin_tag();
    let commit = pin_commit();
    let version = codex_version_from_tag(&tag)
        .unwrap_or_else(|error| panic!("el tag del fixture no es estable rust-vX.Y.Z: {error}"));
    let prefix = raw_prefix(&commit);

    let mut sources = vec![
        SourceRef::pinned(
            tag_ref_url(),
            tag.clone(),
            hex_digest(TAG_REF_JSON.as_bytes()),
            EvidenceKind::ReleaseMetadata,
        ),
        SourceRef::pinned(
            tag_object_url(),
            tag.clone(),
            hex_digest(TAG_OBJECT_JSON.as_bytes()),
            EvidenceKind::ReleaseMetadata,
        ),
        SourceRef::pinned(
            format!("{prefix}/Cargo.toml"),
            commit.clone(),
            hex_digest(PINNED_CARGO_TOML.as_bytes()),
            EvidenceKind::PackageManifest,
        ),
    ];
    for (path, bytes) in GUARD_FILES {
        sources.push(SourceRef::pinned(
            format!("{prefix}/{path}"),
            commit.clone(),
            hex_digest(bytes.as_bytes()),
            EvidenceKind::BundleHash,
        ));
    }
    sources.push(SourceRef::pinned(
        format!("{prefix}/models-manager/models.json"),
        commit.clone(),
        hex_digest(PINNED_MODELS_JSON.as_bytes()),
        EvidenceKind::RuntimeCatalog,
    ));

    let catalog_models = CodexCatalog::parse(PINNED_MODELS_JSON)
        .unwrap_or_else(|error| panic!("el catálogo embebido del pin es inválido: {error}"))
        .models
        .len();
    let profile = build_profile(
        &ResolvedPin {
            release_tag: tag,
            release_version: version.clone(),
            commit,
        },
        catalog_models,
    );
    let identity = ClientIdentity::versioned(version, IDENTITY_ORIGIN_OFFICIAL);

    let candidate = Candidate::without_catalog(
        ContractId::Codex,
        identity,
        CODEX_RUNTIME_RULE_SET_ID,
        CODEX_RUNTIME_EXTRACTOR_VERSION,
        sources,
        profile,
    )
    .with_catalog(PINNED_MODELS_JSON.as_bytes().to_vec(), "json");

    Snapshot::from_candidate(candidate)
        .unwrap_or_else(|error| panic!("la semilla embebida es inválida: {error}"))
}

/// Semilla embebida del pin: los fixtures reales compilados en el binario.
static SEED: LazyLock<Snapshot<CodexProfile>> = LazyLock::new(embedded_seed);

/// Validador del contrato Codex.
///
/// Fail-closed sobre las reglas compiladas: la versión debe ser semver
/// estable derivada del tag, el commit un sha completo, los guards del pin
/// ([`CODEX_REGION_GUARDS`]) probados por una fuente `BundleHash` sobre
/// **el mismo commit**, y el catálogo presente y parseable. Cualquier hueco
/// es `Err`: nunca se activa por omisión.
pub struct CodexContractValidator;

impl ContractValidator<CodexProfile> for CodexContractValidator {
    fn rule_set_id(&self) -> &'static str {
        CODEX_RUNTIME_RULE_SET_ID
    }

    fn extractor_version(&self) -> u32 {
        CODEX_RUNTIME_EXTRACTOR_VERSION
    }

    #[allow(clippy::too_many_lines)]
    fn validate(
        &self,
        candidate: &Candidate<CodexProfile>,
        _previous: Option<&Snapshot<CodexProfile>>,
        _limits: &ContractLimits,
    ) -> Result<(), ContractError> {
        check_compiled_rules(
            candidate,
            CODEX_RUNTIME_RULE_SET_ID,
            CODEX_RUNTIME_EXTRACTOR_VERSION,
        )?;

        let version = parse_version(&candidate.payload.version)?;
        if version.channel != VersionChannel::Stable {
            return Err(ContractError::Rejected(format!(
                "la versión {:?} no es del canal estable",
                candidate.payload.version
            )));
        }
        let expected_tag = format!("rust-v{}", version.normalized);
        if candidate.payload.release_tag != expected_tag {
            return Err(ContractError::Rejected(format!(
                "el candidato anuncia el tag {:?} para la versión {:?}: se esperaba {:?}",
                candidate.payload.release_tag, candidate.payload.version, expected_tag
            )));
        }

        let commit = &candidate.payload.commit;
        if !super::codex_runtime::is_commit_sha(commit) {
            return Err(ContractError::Rejected(format!(
                "la revisión fijada {commit:?} no es un sha de commit completo"
            )));
        }

        // Cada región de guard del pin debe tener una fuente BundleHash sobre
        // el mismo commit: sin procedencia no hay adopción.
        let mut guard_paths: Vec<&'static str> =
            CODEX_REGION_GUARDS.iter().map(|guard| guard.path).collect();
        guard_paths.sort_unstable();
        guard_paths.dedup();
        for path in guard_paths {
            let supported = candidate.sources.iter().any(|source| {
                source.evidence == EvidenceKind::BundleHash
                    && source.revision.as_deref() == Some(commit.as_str())
                    && source.url.ends_with(&format!("{commit}/codex-rs/{path}"))
            });
            if !supported {
                return Err(ContractError::Rejected(format!(
                    "el pin no prueba guards para {path} en el commit {commit}"
                )));
            }
        }

        match candidate.catalog_ref.as_ref() {
            Some(_reference) => match candidate.catalog_body() {
                Some(bytes) => {
                    let text = std::str::from_utf8(bytes).map_err(|error| {
                        ContractError::Rejected(format!("el catálogo no es utf-8: {error}"))
                    })?;
                    let catalog = CodexCatalog::parse(text).map_err(|error| {
                        ContractError::Rejected(format!("el catálogo es inválido: {error}"))
                    })?;
                    if catalog.models.is_empty() {
                        return Err(ContractError::Rejected(
                            "el catálogo del pin está vacío".to_string(),
                        ));
                    }
                }
                None => {
                    // Referencia sin bytes: legal si ya están persistidos, los
                    // verifica el store por digest.
                }
            },
            None => {
                return Err(ContractError::Rejected(
                    "el perfil Codex requiere catálogo para activarse".to_string(),
                ));
            }
        }
        Ok(())
    }
}

/// Piezas concretas del contrato Codex para [`ProviderRuntime`].
#[doc(hidden)]
pub struct CodexProvider;

impl ContractProvider for CodexProvider {
    type Profile = CodexProfile;
    type Driver = CodexReleaseDriver;

    const CONTRACT_ID: ContractId = ContractId::Codex;

    fn seed() -> Snapshot<CodexProfile> {
        (*SEED).clone()
    }

    fn validator() -> Arc<dyn ContractValidator<CodexProfile>> {
        Arc::new(CodexContractValidator)
    }

    fn driver(
        transport: Arc<dyn ContractTransport>,
        cache: Arc<TransportCache>,
        limits: ContractLimits,
    ) -> Arc<CodexReleaseDriver> {
        Arc::new(CodexReleaseDriver::new(transport, cache, limits))
    }

    async fn refresh(
        driver: Arc<CodexReleaseDriver>,
    ) -> Result<Option<Candidate<CodexProfile>>, ContractError> {
        driver.refresh().await
    }
}

/// El perfil Codex no tiene puerta de identidad: un candidato activado siempre
/// puede elevar su versión anunciada.
impl ActivatableVersion for CodexProfile {
    fn activatable_version(&self) -> Option<String> {
        Some(self.version.clone())
    }
}

/// Runtime del contrato Codex: delega en
/// [`GenericContractRuntime`](crate::client_contract::GenericContractRuntime) a
/// través del facade compartido [`ProviderRuntime`].
pub type CodexContractRuntime = ProviderRuntime<CodexProvider>;

/// Raíz por defecto del LKG en disco.
fn default_store_root() -> PathBuf {
    default_store_root_for("OPENPROXY_CODEX_STORE_DIR")
}

/// Runtime global del contrato Codex.
static RUNTIME: LazyLock<CodexContractRuntime> =
    LazyLock::new(|| CodexContractRuntime::new(default_store_root()));

/// El runtime global, accesible desde el server sin reconstruir nada.
#[must_use]
pub fn runtime() -> &'static CodexContractRuntime {
    &RUNTIME
}

/// Carga (o sirve) el snapshot activo del runtime global: LKG en disco o
/// semilla embebida del pin.
///
/// # Errors
/// Errores del store (LKG corrupto o semilla inválida).
pub fn ensure_loaded() -> Result<Arc<Snapshot<CodexProfile>>, ContractError> {
    if let Some(snapshot) = RUNTIME.snapshot() {
        return Ok(snapshot);
    }
    RUNTIME.load()
}

/// El snapshot activo del runtime global, si ya está cargado.
#[must_use]
pub fn current_snapshot() -> Option<Arc<Snapshot<CodexProfile>>> {
    RUNTIME.snapshot()
}

/// La versión de cliente del snapshot activo, si ya está cargado.
#[must_use]
pub fn activated_version() -> Option<String> {
    RUNTIME
        .snapshot()
        .map(|snapshot| snapshot.payload.version.clone())
}

/// Refresca el contrato contra GitHub con el transporte del upstream y
/// devuelve la versión recién activada (`None` si no hubo activación nueva o
/// el candidato no verificó los guards: la generación activa permanece).
#[cfg(feature = "upstream-hyper")]
pub async fn refresh_validated(
    upstream_client: &Arc<crate::upstream::UpstreamClient>,
) -> Option<String> {
    use crate::client_contract::UpstreamHttpTransport;

    let _ = ensure_loaded();
    let transport = Arc::new(UpstreamHttpTransport::new(Arc::clone(upstream_client)));
    RUNTIME.refresh_validated(transport).await
}

/// Sin transporte upstream embebido no hay refresh de red: el runtime global
/// sigue sirviendo la generación activa.
#[cfg(not(feature = "upstream-hyper"))]
pub use shared::refresh_validated_no_upstream as refresh_validated;

#[cfg(test)]
mod tests;
