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
    Candidate, ClientIdentity, ContractError, ContractId, ContractLimits, ContractStore,
    ContractTransport, ContractValidator, EvidenceKind, GenericContractRuntime,
    IDENTITY_ORIGIN_OFFICIAL, RefreshReport, Snapshot, SourceRef, TransportCache, VersionChannel,
    hex_digest, parse_version,
};

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
        if candidate.rule_set_id != CODEX_RUNTIME_RULE_SET_ID {
            return Err(ContractError::Rejected(format!(
                "el candidato declara el rule set {:?} y el runtime compilado es {:?}",
                candidate.rule_set_id, CODEX_RUNTIME_RULE_SET_ID
            )));
        }
        if candidate.extractor_version != CODEX_RUNTIME_EXTRACTOR_VERSION {
            return Err(ContractError::Rejected(format!(
                "el candidato usa el extractor v{} y el runtime compilado es v{}",
                candidate.extractor_version, CODEX_RUNTIME_EXTRACTOR_VERSION
            )));
        }

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

/// Runtime del contrato Codex: delega en [`GenericContractRuntime`].
pub struct CodexContractRuntime {
    inner: GenericContractRuntime<CodexProfile, CodexReleaseDriver>,
}

impl CodexContractRuntime {
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
                ContractId::Codex,
                root,
                limits,
                (*SEED).clone(),
                Arc::new(CodexContractValidator),
                |transport, cache, limits| {
                    Arc::new(CodexReleaseDriver::new(transport, cache, limits))
                },
            ),
        }
    }

    /// El store tipado, para commits directos de un candidato verificado.
    #[must_use]
    pub fn store(&self) -> &ContractStore<CodexProfile> {
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
    pub fn store_handle(&self) -> Arc<ContractStore<CodexProfile>> {
        self.inner.store_handle()
    }

    /// Driver de una ronda de refresh sobre `transport`, con la cache y los
    /// límites de este runtime.
    ///
    /// La cache es la del runtime: el refresher no arranca con una cache fría
    /// paralela, sino con los mismos condicionales que ya se usaron.
    #[must_use]
    pub fn release_driver(&self, transport: Arc<dyn ContractTransport>) -> Arc<CodexReleaseDriver> {
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
    pub fn load(&self) -> Result<Arc<Snapshot<CodexProfile>>, ContractError> {
        self.inner.load()
    }

    /// El snapshot ya cargado, si el runtime ya sirvió uno.
    #[must_use]
    pub fn snapshot(&self) -> Option<Arc<Snapshot<CodexProfile>>> {
        self.inner.snapshot()
    }

    /// Activa un candidato verificado (anti-downgrade + persistencia).
    ///
    /// # Errors
    /// Errores del store o del validador.
    pub fn commit(
        &self,
        candidate: &Candidate<CodexProfile>,
    ) -> Result<Option<Arc<Snapshot<CodexProfile>>>, ContractError> {
        self.inner.commit(candidate)
    }

    /// Resuelve el pin contra `transport` y activa el candidato si verifica
    /// los guards; si no, la generación activa no cambia.
    ///
    /// # Errors
    /// Errores del driver o del store.
    pub async fn refresh(
        &self,
        transport: Arc<dyn ContractTransport>,
    ) -> Result<RefreshReport<CodexProfile>, ContractError> {
        self.inner
            .refresh_with(transport, |driver| async move { driver.refresh().await })
            .await
    }
}

/// Raíz por defecto del LKG en disco.
fn default_store_root() -> PathBuf {
    if let Ok(dir) = std::env::var("OPENPROXY_CODEX_STORE_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".openproxy").join("contracts")
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
    match RUNTIME.refresh(transport).await {
        Ok(RefreshReport::Activated { snapshot, .. }) => Some(snapshot.payload.version.clone()),
        Ok(RefreshReport::Unchanged { .. } | RefreshReport::Rejected { .. }) => None,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "el refresh del contrato Codex no verificó; se conserva la generación activa"
            );
            None
        }
    }
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
    use crate::client_contract::providers::codex_runtime::is_commit_sha;
    use crate::client_contract::validate_candidate_shape;
    use crate::client_contract::{FetchRequest, FetchResponse};
    use std::collections::HashMap;
    use std::fs;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Transporte que responde desde un mapa de URL. Ninguna petición sale
    /// del proceso; una URL ausente se comporta como `404`.
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
        let dir = std::env::temp_dir().join(format!("openproxy-codex-state-{name}-{stamp}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ok_json(body: &str) -> FetchResponse {
        FetchResponse {
            status: 200,
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn ok_raw(bytes: &[u8]) -> FetchResponse {
        FetchResponse {
            status: 200,
            headers: Vec::new(),
            body: bytes.to_vec(),
        }
    }

    /// Mapa de red completo del pin, parametrizado por tag y commit: la misma
    /// forma que sirve GitHub para el pin.
    fn pin_map(tag: &str, commit: &str, overrides: Vec<(&str, FetchResponse)>) -> MapTransport {
        let tag_object_sha = pin_tag_object_sha();
        let mut entries: Vec<(String, FetchResponse)> = vec![
            (
                "https://api.github.com/repos/openai/codex/releases/latest".to_string(),
                ok_json(&format!(
                    r#"{{"tag_name":"{tag}","name":"{tag}","prerelease":false,"draft":false}}"#
                )),
            ),
            (
                format!("https://api.github.com/repos/openai/codex/git/refs/tags/{tag}"),
                ok_json(&format!(
                    r#"{{"object":{{"sha":"{tag_object_sha}","type":"tag"}}}}"#
                )),
            ),
            (
                format!("https://api.github.com/repos/openai/codex/git/tags/{tag_object_sha}"),
                ok_json(&format!(
                    r#"{{"object":{{"sha":"{commit}","type":"commit"}}}}"#
                )),
            ),
            (
                format!(
                    "https://raw.githubusercontent.com/openai/codex/{commit}/codex-rs/Cargo.toml"
                ),
                ok_raw(PINNED_CARGO_TOML.as_bytes()),
            ),
            (
                format!(
                    "https://raw.githubusercontent.com/openai/codex/{commit}/codex-rs/models-manager/models.json"
                ),
                ok_raw(PINNED_MODELS_JSON.as_bytes()),
            ),
        ];
        for (path, bytes) in GUARD_FILES {
            entries.push((
                format!("https://raw.githubusercontent.com/openai/codex/{commit}/codex-rs/{path}"),
                ok_raw(bytes.as_bytes()),
            ));
        }
        let map = MapTransport::with(entries);
        for (relative, response) in overrides {
            map.set(
                format!(
                    "https://raw.githubusercontent.com/openai/codex/{commit}/codex-rs/{relative}"
                ),
                response,
            );
        }
        map
    }

    /// El seed embebido es un snapshot válido del pin: procedencia por byte y
    /// catálogo incluido.
    #[test]
    fn the_embedded_seed_is_a_valid_pinned_snapshot() {
        let seed = embedded_seed();
        let commit = pin_commit();
        assert_eq!(seed.contract_id, ContractId::Codex);
        assert_eq!(seed.payload.version, "0.162.1");
        assert_eq!(seed.payload.release_tag, "rust-v0.162.1");
        assert_eq!(seed.payload.commit, commit);
        assert_eq!(seed.payload.catalog_models, 11);
        assert_eq!(
            seed.sources.len(),
            3 + GUARD_FILES.len() + 1,
            "ref + tag object + Cargo.toml + guards + catálogo"
        );
        assert!(seed.catalog_body().is_some());
        validate_candidate_shape(&seed.as_candidate(), &ContractLimits::default()).unwrap();
    }

    /// Con disco vacío el store sirve la semilla: el proxy arranca con una
    /// generación activa sin tocar la red.
    #[test]
    fn an_empty_store_loads_the_embedded_seed() {
        let dir = temp_dir("seed");
        let runtime = CodexContractRuntime::new(&dir);
        let snapshot = runtime.load().unwrap();
        assert_eq!(snapshot.payload.version, "0.162.1");
        assert_eq!(
            runtime.snapshot().unwrap().payload.version,
            "0.162.1",
            "el snapshot queda cacheado tras load"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// LKG: un candidato activado sobrevive a un reinicio del runtime, con la
    /// misma generación content-addressed.
    #[test]
    fn an_activated_candidate_survives_a_restart() {
        let dir = temp_dir("lkg");
        let runtime = CodexContractRuntime::new(&dir);
        let seed = runtime.load().unwrap();
        let commit = pin_commit();

        let mut candidate = seed.as_candidate();
        candidate.payload.version = "0.163.0".to_string();
        candidate.payload.release_tag = "rust-v0.163.0".to_string();
        candidate.client_identity = ClientIdentity::versioned("0.163.0", IDENTITY_ORIGIN_OFFICIAL);
        for source in &mut candidate.sources {
            source.revision = Some(commit.clone());
        }
        let published = runtime.commit(&candidate).unwrap().expect("activación");
        assert_eq!(published.payload.version, "0.163.0");

        let restarted = CodexContractRuntime::new(&dir).load().unwrap();
        assert_eq!(restarted.payload.version, "0.163.0");
        assert_eq!(restarted.generation, published.generation);
        let _ = fs::remove_dir_all(&dir);
    }

    /// El validador rechaza un candidato cuyo tag no coincide con la versión.
    #[test]
    fn a_candidate_whose_tag_disagrees_with_its_version_is_rejected() {
        let dir = temp_dir("validator-tag");
        let runtime = CodexContractRuntime::new(&dir);
        let seed = runtime.load().unwrap();

        let mut candidate = seed.as_candidate();
        candidate.payload.release_tag = "rust-v0.163.0".to_string();
        let error = runtime.commit(&candidate).unwrap_err();
        assert!(
            matches!(error, ContractError::Rejected(ref message) if message.contains("se esperaba")),
            "got {error:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// El validador rechaza un pin sin procedencia de guards para el commit.
    #[test]
    fn a_pin_without_guard_provenance_is_rejected() {
        let dir = temp_dir("validator-guards");
        let runtime = CodexContractRuntime::new(&dir);
        let seed = runtime.load().unwrap();

        let mut candidate = seed.as_candidate();
        candidate.sources.retain(|source| {
            source.evidence != EvidenceKind::BundleHash
                || !source.url.ends_with("codex-rs/core/src/client.rs")
        });
        let error = runtime.commit(&candidate).unwrap_err();
        assert!(
            matches!(error, ContractError::Rejected(ref message) if message.contains("core/src/client.rs")),
            "got {error:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Un candidato sin catálogo no se activa.
    #[test]
    fn a_candidate_without_a_catalog_is_rejected() {
        let dir = temp_dir("validator-catalog");
        let runtime = CodexContractRuntime::new(&dir);
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

    /// Un candidato por debajo de la generación activa no se activa.
    #[test]
    fn a_downgrade_is_rejected() {
        let dir = temp_dir("downgrade");
        let runtime = CodexContractRuntime::new(&dir);
        let seed = runtime.load().unwrap();

        let mut candidate = seed.as_candidate();
        candidate.payload.version = "0.162.0".to_string();
        candidate.payload.release_tag = "rust-v0.162.0".to_string();
        candidate.client_identity = ClientIdentity::versioned("0.162.0", IDENTITY_ORIGIN_OFFICIAL);
        let error = runtime
            .commit(&candidate)
            .expect_err("un downgrade debe rechazarse");
        assert!(
            matches!(error, ContractError::Downgrade { .. }),
            "got {error:?}"
        );
        assert_eq!(runtime.snapshot().unwrap().payload.version, "0.162.1");
        let _ = fs::remove_dir_all(&dir);
    }

    /// El refresh validado activa una release estable más nueva cuyo commit
    /// verifica los guards compilados, y persiste el LKG.
    #[tokio::test]
    async fn a_newer_stable_release_is_activated_by_refresh() {
        let dir = temp_dir("refresh-activate");
        let runtime = CodexContractRuntime::new(&dir);
        runtime.load().unwrap();

        let cargo = PINNED_CARGO_TOML.replace("version = \"0.162.1\"", "version = \"0.163.0\"");
        let transport = Arc::new(pin_map(
            "rust-v0.163.0",
            &pin_commit(),
            vec![("Cargo.toml", ok_raw(cargo.as_bytes()))],
        ));
        let report = runtime.refresh(transport).await.unwrap();
        match &report {
            RefreshReport::Activated { snapshot, .. } => {
                assert_eq!(snapshot.payload.version, "0.163.0");
            }
            RefreshReport::Unchanged { generation } => {
                panic!("la release nueva debió activarse, got Unchanged({generation})");
            }
            RefreshReport::Rejected { reason } => {
                panic!("la release nueva debió activarse, got Rejected({reason})");
            }
        }
        let restarted = CodexContractRuntime::new(&dir).load().unwrap();
        assert_eq!(restarted.payload.version, "0.163.0");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Una revisión nueva cuyos bytes no verifican los guards se rechaza: la
    /// generación activa permanece.
    #[tokio::test]
    async fn a_refresh_with_broken_guards_keeps_the_active_snapshot() {
        let dir = temp_dir("refresh-guards");
        let runtime = CodexContractRuntime::new(&dir);
        runtime.load().unwrap();

        let client = GUARD_FILES
            .iter()
            .find(|(path, _)| *path == "core/src/client.rs")
            .map(|(_, bytes)| bytes.replace("store: false,", "store: true,"))
            .expect("guard core/src/client.rs embebido");
        let cargo = PINNED_CARGO_TOML.replace("version = \"0.162.1\"", "version = \"0.163.0\"");
        let transport = Arc::new(pin_map(
            "rust-v0.163.0",
            &pin_commit(),
            vec![
                ("Cargo.toml", ok_raw(cargo.as_bytes())),
                ("core/src/client.rs", ok_raw(client.as_bytes())),
            ],
        ));
        let error = runtime.refresh(transport).await.unwrap_err();
        assert!(
            matches!(error, ContractError::Rejected(ref message) if message.contains("guards de región")),
            "got {error:?}"
        );
        assert_eq!(runtime.snapshot().unwrap().payload.version, "0.162.1");
        let _ = fs::remove_dir_all(&dir);
    }

    /// El validador acepta la semilla: es la referencia de "esto sí pasa".
    #[test]
    fn the_validator_accepts_the_embedded_seed() {
        let candidate = embedded_seed().as_candidate();
        CodexContractValidator
            .validate(&candidate, None, &ContractLimits::default())
            .unwrap();
        assert_eq!(
            CodexContractValidator.rule_set_id(),
            CODEX_RUNTIME_RULE_SET_ID
        );
        assert_eq!(
            CodexContractValidator.extractor_version(),
            CODEX_RUNTIME_EXTRACTOR_VERSION
        );
    }

    /// La raíz por defecto del LKG respeta el entorno.
    #[test]
    fn the_default_store_root_follows_the_environment() {
        let previous = std::env::var("OPENPROXY_CODEX_STORE_DIR").ok();
        // SAFETY: el test corre serializado por CODEX_TEST_LOCK, la misma
        // convención que el resto del workspace para mutar el entorno.
        let _guard = crate::spoofer::codex::CODEX_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        unsafe {
            std::env::set_var(
                "OPENPROXY_CODEX_STORE_DIR",
                "/tmp/openproxy-codex-state-env",
            );
        }
        let root = default_store_root();
        match previous {
            Some(value) => unsafe {
                std::env::set_var("OPENPROXY_CODEX_STORE_DIR", value);
            },
            None => unsafe {
                std::env::remove_var("OPENPROXY_CODEX_STORE_DIR");
            },
        }
        assert_eq!(root, PathBuf::from("/tmp/openproxy-codex-state-env"));
    }

    /// El tag y el commit del pin se leen de los fixtures, no de constantes
    /// compiladas.
    #[test]
    fn the_pin_tag_and_commit_come_from_the_fixtures() {
        assert_eq!(pin_tag(), "rust-v0.162.1");
        assert_eq!(codex_version_from_tag(&pin_tag()).unwrap(), "0.162.1");
        assert!(is_commit_sha(&pin_commit()));
        assert!(is_commit_sha(&pin_tag_object_sha()));
    }

    /// El handle del store es el mismo store que el runtime lee: un commit
    /// hecho a través del handle aparece de inmediato en el snapshot activo.
    #[test]
    fn the_store_handle_is_the_store_the_runtime_reads() {
        let dir = temp_dir("store-handle");
        let runtime = CodexContractRuntime::new(&dir);
        let seed = runtime.load().unwrap();
        let handle = runtime.store_handle();
        assert!(Arc::ptr_eq(&handle, &runtime.store_handle()));

        let mut candidate = seed.as_candidate();
        candidate.payload.version = "0.163.0".to_string();
        candidate.payload.release_tag = "rust-v0.163.0".to_string();
        candidate.client_identity = ClientIdentity::versioned("0.163.0", IDENTITY_ORIGIN_OFFICIAL);
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
        let runtime = CodexContractRuntime::new(&dir);
        let transport: Arc<dyn ContractTransport> = Arc::new(MapTransport::default());
        let driver = runtime.release_driver(transport);
        assert!(Arc::ptr_eq(driver.cache(), runtime.cache()));
        let _ = fs::remove_dir_all(&dir);
    }
}
