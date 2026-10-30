use crate::client_contract::providers::codebuddy_runtime::{
    CODEBUDDY_REGISTRY_LATEST_URL, pinned_package_url, pinned_product_url,
};
use crate::client_contract::providers::codebuddy_state::CodeBuddyContractRuntime;
use crate::client_contract::{ContractError, ContractTransport, FetchRequest, FetchResponse};

const REFRESH_REGISTRY_JSON: &str =
    include_str!("../../../../../../fixtures/codebuddy/registry-latest.json");
const REFRESH_PACKAGE_JSON: &str =
    include_str!("../../../../../../fixtures/codebuddy-extracted/package.json");
const REFRESH_PRODUCT_JSON: &str =
    include_str!("../../../../../../fixtures/codebuddy-extracted/product.json");

/// Transporte hermético del refresh: responde desde un mapa de URL y ninguna
/// petición sale del proceso. Una URL ausente se comporta como 404.
struct RefreshTransport {
    responses: std::sync::Mutex<std::collections::HashMap<String, FetchResponse>>,
}

impl RefreshTransport {
    fn with(entries: Vec<(String, FetchResponse)>) -> Self {
        Self {
            responses: std::sync::Mutex::new(entries.into_iter().collect()),
        }
    }
}

impl ContractTransport for RefreshTransport {
    fn fetch(
        &self,
        request: &FetchRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<FetchResponse, ContractError>>
                + Send
                + '_,
        >,
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

fn refresh_ok_json(body: &[u8]) -> FetchResponse {
    FetchResponse {
        status: 200,
        headers: Vec::new(),
        body: body.to_vec(),
    }
}

fn refresh_temp_dir(name: &str) -> std::path::PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("openproxy-codebuddy-refresh-{name}-{stamp}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Ruta de refresh validada sobre runtime hermético: la metadata que anuncia
/// otra versión es rechazada y la generación activa se conserva.
#[tokio::test]
async fn test_codebuddy_refresh_rejects_metadata_only_elevation() {
    let dir = refresh_temp_dir("metadata-only");
    let runtime = CodeBuddyContractRuntime::new(&dir);
    let seed = runtime.load().expect("seed embebido");

    let transport = std::sync::Arc::new(RefreshTransport::with(vec![
        (
            CODEBUDDY_REGISTRY_LATEST_URL.to_string(),
            refresh_ok_json(br#"{"name":"@tencent-ai/codebuddy-code","version":"2.199.9"}"#),
        ),
        (
            pinned_package_url("2.199.9"),
            refresh_ok_json(br#"{"name":"@tencent-ai/codebuddy-code","version":"2.199.9"}"#),
        ),
        (pinned_product_url("2.199.9"), refresh_ok_json(b"{}")),
    ]));
    let elevated = runtime.refresh_validated(transport).await;
    assert_eq!(elevated, None, "metadata sola no eleva la identidad");
    assert_eq!(
        runtime.snapshot().unwrap().generation,
        seed.generation,
        "la generación activa no cambia"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// La ruta de refresh validada publica el catálogo como overlay, pero sin
/// correspondencia paquete→commit verificada la versión anunciada no sube.
#[tokio::test]
async fn test_codebuddy_refresh_pinned_round_does_not_elevate_the_version() {
    let dir = refresh_temp_dir("pinned-round");
    let runtime = CodeBuddyContractRuntime::new(&dir);
    let seed = runtime.load().expect("seed embebido");
    let version = seed.payload.version.clone();

    let transport = std::sync::Arc::new(RefreshTransport::with(vec![
        (
            CODEBUDDY_REGISTRY_LATEST_URL.to_string(),
            refresh_ok_json(REFRESH_REGISTRY_JSON.as_bytes()),
        ),
        (
            pinned_package_url(&version),
            refresh_ok_json(REFRESH_PACKAGE_JSON.as_bytes()),
        ),
        (
            pinned_product_url(&version),
            refresh_ok_json(REFRESH_PRODUCT_JSON.as_bytes()),
        ),
    ]));
    let elevated = runtime.refresh_validated(transport).await;
    assert_eq!(elevated, None, "sin gitHead no hay identidad activable");
    let snapshot = runtime.snapshot().unwrap();
    assert_eq!(snapshot.payload.version, version);
    assert!(!snapshot.payload.identity_activatable);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fallas de adquisición son fail-closed: `None`, sin pánico y sin cambiar la
/// generación activa.
#[tokio::test]
async fn test_codebuddy_refresh_failures_are_fail_closed() {
    let dir = refresh_temp_dir("failures");
    let runtime = CodeBuddyContractRuntime::new(&dir);
    let seed = runtime.load().expect("seed embebido");

    // 429 en el registro: error de adquisición, generación intacta.
    let throttled = std::sync::Arc::new(RefreshTransport::with(vec![(
        CODEBUDDY_REGISTRY_LATEST_URL.to_string(),
        FetchResponse {
            status: 429,
            headers: Vec::new(),
            body: Vec::new(),
        },
    )]));
    assert_eq!(runtime.refresh_validated(throttled).await, None);

    // 200 con un cuerpo que no es JSON: el parseo falla, generación intacta.
    let not_json = std::sync::Arc::new(RefreshTransport::with(vec![(
        CODEBUDDY_REGISTRY_LATEST_URL.to_string(),
        refresh_ok_json(b"NotJson"),
    )]));
    assert_eq!(runtime.refresh_validated(not_json).await, None);

    assert_eq!(runtime.snapshot().unwrap().generation, seed.generation);
    let _ = std::fs::remove_dir_all(&dir);
}
