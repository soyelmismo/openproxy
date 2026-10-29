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
            format!("https://raw.githubusercontent.com/openai/codex/{commit}/codex-rs/Cargo.toml"),
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
            format!("https://raw.githubusercontent.com/openai/codex/{commit}/codex-rs/{relative}"),
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
        .unwrap_or_else(|poison| {
            crate::spoofer::codex::CODEX_TEST_LOCK.clear_poison();
            poison.into_inner()
        });
    // SAFETY: CODEX_TEST_LOCK guarantees exclusive single-threaded access to environment variables.
    unsafe {
        std::env::set_var(
            "OPENPROXY_CODEX_STORE_DIR",
            "/tmp/openproxy-codex-state-env",
        );
    }
    let root = default_store_root();
    match previous {
        Some(value) => {
            // SAFETY: CODEX_TEST_LOCK guarantees exclusive single-threaded access to environment variables.
            unsafe {
                std::env::set_var("OPENPROXY_CODEX_STORE_DIR", value);
            }
        }
        None => {
            // SAFETY: CODEX_TEST_LOCK guarantees exclusive single-threaded access to environment variables.
            unsafe {
                std::env::remove_var("OPENPROXY_CODEX_STORE_DIR");
            }
        }
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
