#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::client_contract::providers::codebuddy_runtime::{
    CODEBUDDY_REGISTRY_LATEST_URL, CODEBUDDY_UNPKG_PREFIX, CodeBuddyReleaseDriver,
    pinned_package_url, pinned_product_url,
};
use crate::client_contract::{
    FetchRequest, FetchResponse, RefreshReport, validate_candidate_shape,
};
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
        matches!(error, ContractError::Rejected(ref message) if message.contains("without compiled guards")),
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
        matches!(error, ContractError::Rejected(ref message) if message.contains("metadata alone")),
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
