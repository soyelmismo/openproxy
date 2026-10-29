use super::*;

use std::collections::HashMap;
use std::sync::Mutex;

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
    std::fs::read(root.join(relative)).unwrap_or_else(|error| panic!("fixture {relative}: {error}"))
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
