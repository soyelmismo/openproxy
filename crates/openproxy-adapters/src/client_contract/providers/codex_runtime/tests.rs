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

/// Tag estable del pin compilado.
const PINNED_TAG: &str = "rust-v0.162.1";

/// Sha del annotated tag object del pin (visible en
/// `fixtures/codex-tag-object.json`): a donde apunta `refs/tags`.
const TAG_OBJECT_SHA: &str = "7a078f6bb4ce0dc2a81408b334ce52979d92c603";

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
    std::fs::read(root.join(relative)).unwrap_or_else(|error| panic!("fixture {relative}: {error}"))
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
    full_map_tagged(PINNED_TAG, commit, overrides)
}

/// Respuestas de un pin completo para un tag estable cualquiera: la
/// versión anunciada y el `[workspace.package]` derivan del tag, y el
/// mismo tag object desreferencia al commit servido.
fn full_map_tagged(tag: &str, commit: &str, overrides: Vec<(&str, FetchResponse)>) -> MapTransport {
    let mut entries: Vec<(String, FetchResponse)> = vec![
        (
            CODEX_RELEASES_LATEST_URL.to_string(),
            ok_json(&format!(
                r#"{{"tag_name":"{tag}","name":"{tag}","prerelease":false,"draft":false}}"#
            )),
        ),
        (
            format!("https://api.github.com/repos/openai/codex/git/refs/tags/{tag}"),
            ok_json(&format!(
                r#"{{"object":{{"sha":"{TAG_OBJECT_SHA}","type":"tag"}}}}"#
            )),
        ),
        (
            format!("https://api.github.com/repos/openai/codex/git/tags/{TAG_OBJECT_SHA}"),
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
        let url =
            format!("https://raw.githubusercontent.com/openai/codex/{commit}/codex-rs/{relative}");
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
    let body = br#"{"tag_name":"rust-v0.162.1","name":"0.162.1","prerelease":false,"draft":false}"#;
    let meta = parse_release_metadata(body).unwrap();
    assert_eq!(meta.tag, "rust-v0.162.1");
    assert_eq!(meta.version, "0.162.1");
    assert!(!meta.prerelease);
    assert!(!meta.draft);
}

#[test]
fn release_metadata_is_fail_closed_without_channel_fields() {
    let meta = parse_release_metadata(br#"{"tag_name":"rust-v0.162.1","name":"0.162.1"}"#).unwrap();
    assert!(meta.prerelease, "un campo ausente se trata como prerelease");
    assert!(meta.draft, "un campo ausente se trata como draft");
}

#[test]
fn release_metadata_rejects_html_and_missing_tag() {
    assert!(parse_release_metadata(b"<html>nope</html>").is_err());
    assert!(parse_release_metadata(br#"{"name":"0.162.1"}"#).is_err());
}

/// Una release estable más nueva que el pin compilado se adopta mientras
/// sus bytes verifiquen los guards: la versión del candidato sale del tag
/// y el `[workspace.package]` debe coincidir con ella.
#[tokio::test]
async fn a_newer_stable_release_is_adopted_when_the_pin_still_verifies() {
    let new_tag = "rust-v0.163.0";
    let cargo = String::from_utf8(fixture_bytes("codex-rust-v0.162.1/codex-rs/Cargo.toml"))
        .unwrap()
        .replace("version = \"0.162.1\"", "version = \"0.163.0\"");
    let candidate = driver(full_map_tagged(
        new_tag,
        PIN_COMMIT,
        vec![("Cargo.toml", ok_raw(cargo.as_bytes()))],
    ))
    .refresh()
    .await
    .unwrap()
    .expect("un candidato de la release nueva");
    assert_eq!(candidate.payload.version, "0.163.0");
    assert_eq!(candidate.payload.release_tag, new_tag);
    assert_eq!(candidate.payload.commit, PIN_COMMIT);
    assert_eq!(
        candidate.client_identity.version.as_deref(),
        Some("0.163.0")
    );
}

/// Una revisión nueva cuyos bytes NO verifican los guards compilados se
/// rechaza: sin guards para la revisión no hay adopción, y el LKG del
/// store permanece intacto.
#[tokio::test]
async fn a_new_revision_with_broken_guards_is_rejected() {
    let new_tag = "rust-v0.163.0";
    let new_commit = "b7c2e19f4a8d6035c1e2f7a9d4b8063512cfe947";
    let cargo = String::from_utf8(fixture_bytes("codex-rust-v0.162.1/codex-rs/Cargo.toml"))
        .unwrap()
        .replace("version = \"0.162.1\"", "version = \"0.163.0\"");
    let client = String::from_utf8(fixture_bytes(
        "codex-rust-v0.162.1/codex-rs/core/src/client.rs",
    ))
    .unwrap()
    .replace("store: false,", "store: true,");
    let transport = full_map_tagged(
        new_tag,
        new_commit,
        vec![
            ("Cargo.toml", ok_raw(cargo.as_bytes())),
            ("core/src/client.rs", ok_raw(client.as_bytes())),
        ],
    );
    let error = driver(transport).refresh().await.unwrap_err();
    assert!(
        matches!(error, ContractError::Rejected(ref message) if message.contains("guards de región")),
        "un guard roto en el commit nuevo debe rechazar el candidato, got {error:?}"
    );
}

/// La versión del contrato sale del tag, no del campo `name` de la API de
/// GitHub, que es texto libre editable.
#[test]
fn the_version_comes_from_the_tag_not_from_the_release_name() {
    let meta = parse_release_metadata(
            br#"{"tag_name":"rust-v0.163.0","name":"Codex 9.9.9 (editable)","prerelease":false,"draft":false}"#,
        )
        .unwrap();
    assert_eq!(meta.version, "0.163.0");
    assert_eq!(meta.tag, "rust-v0.163.0");
}

/// Un tag fuera de la forma estable `rust-vX.Y.Z` se rechaza antes de
/// cualquier salto de red: fail-closed.
#[test]
fn a_tag_outside_the_stable_rust_v_shape_is_rejected() {
    for tag in [
        "rust-v0.163.0-rc.1",
        "rust-v0.163",
        "codex-v0.163.0",
        "v0.163.0",
        "0.163.0",
    ] {
        let error = codex_version_from_tag(tag).unwrap_err();
        assert!(
            matches!(error, ContractError::Rejected(_)),
            "{tag}: got {error:?}"
        );
    }
    assert_eq!(codex_version_from_tag("rust-v0.163.0").unwrap(), "0.163.0");
    assert_eq!(codex_version_from_tag(PINNED_TAG).unwrap(), "0.162.1");
}

/// La versión esperada del workspace sigue al tag de la release, no a una
/// constante compilada.
#[test]
fn expected_workspace_version_follows_the_release_tag() {
    let pin = ResolvedPin {
        release_tag: "rust-v0.163.0".to_string(),
        release_version: "0.163.0".to_string(),
        commit: PIN_COMMIT.to_string(),
    };
    assert_eq!(pin.expected_workspace_version(), "0.163.0");
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
    let chained = r#"{"object":{"sha":"2222222222222222222222222222222222222222","type":"tag"}}"#;
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

#[path = "tests_extraction.rs"]
mod tests_extraction;
