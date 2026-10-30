use super::*;

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
        format!("https://api.github.com/repos/openai/codex/git/refs/tags/{PINNED_TAG}"),
        format!("https://api.github.com/repos/openai/codex/git/tags/{TAG_OBJECT_SHA}"),
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
