//! Tests de fuentes release-pinned para Codex, CodeBuddy y OpenCode.
//!
//! Los tests cargan fixtures reales descargadas y verificadas por hash. No
//! ejecutan código remoto ni dependen de `main`/`latest`: cada fixture está
//! fijada a una revisión concreta registrada en `fixtures/manifest.json`.
//!
//! Las fixtures grandes (tarball npm de ~56 MB y bundle minificado de ~27 MB)
//! no se commitean: se referencian por ruta y hash. La verificación del bundle
//! es un test ignorado explícitamente y falla si se ejecuta sin el artefacto.

#![allow(
    clippy::all,
    clippy::pedantic,
    clippy::unwrap_used,
    clippy::expect_used
)]

use openproxy_adapters::client_contract::providers::{
    PackageCommitCorrespondence, codebuddy, codex, opencode,
};

/// Raíz de fixtures relativa al manifest del crate.
fn fixture_root() -> std::path::PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    std::path::Path::new(manifest_dir)
        .join("../../fixtures")
        .canonicalize()
        .expect("directorio de fixtures accesible")
}

/// Lee una fixture o devuelve `None` si no está disponible.
fn read_fixture(rel: &str) -> Option<String> {
    let path = fixture_root().join(rel);
    std::fs::read_to_string(path).ok()
}

/// Lee bytes de una fixture o devuelve `None` si no está disponible.
fn read_fixture_bytes(rel: &str) -> Option<Vec<u8>> {
    let path = fixture_root().join(rel);
    std::fs::read(path).ok()
}

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

#[test]
fn codex_catalog_parses_from_pinned_release() {
    let json = read_fixture("codex-rust-v0.162.1/codex-rs/models-manager/models.json")
        .expect("models.json de rust-v0.162.1");
    let catalog = codex::CodexCatalog::parse(&json).expect("catálogo Codex válido");

    assert_eq!(catalog.models.len(), 11);
    assert!(catalog.find("codex-auto-review").is_some());
    assert!(
        catalog.find("gpt-5.4").is_none(),
        "gpt-5.4 no debe inferirse en 0.162.1"
    );
    assert!(
        catalog.models.iter().all(|m| m.supported_in_api),
        "todos los modelos del pin declaran supported_in_api"
    );
}

#[test]
fn codex_release_tag_matches_workspace_version() {
    let cargo = read_fixture("codex-rust-v0.162.1/codex-rs/Cargo.toml")
        .expect("Cargo.toml de rust-v0.162.1");
    let expected = format!("version = \"{}\"", codex::CODEX_WORKSPACE_VERSION);
    assert!(
        cargo.contains(&expected),
        "el workspace debe declarar {expected}"
    );
    assert_eq!(codex::CODEX_RELEASE_TAG, "rust-v0.162.1");
}

#[test]
fn codex_region_guards_hold_on_pinned_sources() {
    let base = fixture_root().join("codex-rust-v0.162.1/codex-rs");
    let mut files: Vec<(&'static str, String)> = Vec::new();

    for guard in codex::CODEX_REGION_GUARDS {
        if files.iter().any(|(p, _)| *p == guard.path) {
            continue;
        }
        let content = std::fs::read_to_string(base.join(guard.path))
            .unwrap_or_else(|e| panic!("leyendo {}: {e}", guard.path));
        files.push((guard.path, content));
    }

    let refs: Vec<(&'static str, &str)> = files.iter().map(|(p, c)| (*p, c.as_str())).collect();
    codex::verify_region_guards(&refs).expect("guards de región Codex");
}

// ---------------------------------------------------------------------------
// CodeBuddy
// ---------------------------------------------------------------------------

#[test]
fn codebuddy_catalog_parses_from_pinned_package() {
    let json = read_fixture("codebuddy-extracted/product.json").expect("product.json de 2.164.0");
    let catalog = codebuddy::CodeBuddyCatalog::parse(&json).expect("catálogo CodeBuddy válido");

    assert_eq!(catalog.models.len(), 22);
    assert!(catalog.default_model().is_some());
    assert_eq!(
        catalog
            .models
            .iter()
            .filter(|m| m.vendor == codebuddy::CodeBuddyVendor::E)
            .count(),
        12
    );
}

#[test]
fn codebuddy_package_version_is_pinned() {
    let pkg = read_fixture("codebuddy-extracted/package.json").expect("package.json de 2.164.0");
    let parsed: serde_json::Value = serde_json::from_str(&pkg).expect("package.json válido");
    assert_eq!(parsed["name"], codebuddy::CODEBUDDY_PACKAGE);
    assert_eq!(parsed["version"], codebuddy::CODEBUDDY_VERSION);
}

#[test]
fn codebuddy_identity_is_not_activatable_from_metadata_alone() {
    // El paquete npm no expone `gitHead`: la procedencia no es verificable y
    // la identidad no debe activarse solo por metadata.
    let correspondence = PackageCommitCorrespondence::Unverified {
        reason: "el paquete npm @tencent-ai/codebuddy-code no expone gitHead".to_string(),
    };
    assert!(!correspondence.is_verified());
}

#[test]
#[ignore = "requires the integrity-verified CodeBuddy bundle, not committed"]
fn codebuddy_bundle_guards_hold_when_bundle_present() {
    let bundle = read_fixture_bytes("codebuddy-extracted/codebuddy.js")
        .expect("bundle required: fetch and verify the pinned npm artifact first");
    codebuddy::verify_bundle_guards(&bundle).expect("guards de bundle CodeBuddy");
}

// ---------------------------------------------------------------------------
// OpenCode
// ---------------------------------------------------------------------------

#[test]
fn opencode_catalogs_are_separate_and_typed() {
    let zen_json = read_fixture("opencode-runtime/zen-models.json").expect("catálogo Zen");
    let go_json = read_fixture("opencode-runtime/go-models.json").expect("catálogo Go");

    let zen = opencode::OpenCodeCatalog::parse(opencode::OpenCodeSurface::Zen, &zen_json)
        .expect("catálogo Zen válido");
    let go = opencode::OpenCodeCatalog::parse(opencode::OpenCodeSurface::Go, &go_json)
        .expect("catálogo Go válido");

    assert_eq!(zen.models.len(), 86);
    assert_eq!(go.models.len(), 39);
    assert_eq!(zen.surface, opencode::OpenCodeSurface::Zen);
    assert_eq!(go.surface, opencode::OpenCodeSurface::Go);
}

#[test]
fn opencode_routing_comes_from_documented_table_only() {
    let zen_json = read_fixture("opencode-runtime/zen-models.json").expect("catálogo Zen");
    let zen = opencode::OpenCodeCatalog::parse(opencode::OpenCodeSurface::Zen, &zen_json)
        .expect("catálogo Zen válido");

    assert_eq!(
        zen.route_family("gpt-6-sol"),
        opencode::OpenCodeRouteFamily::Responses
    );
    assert_eq!(
        zen.route_family("claude-haiku-5-5"),
        opencode::OpenCodeRouteFamily::Messages
    );
    // Un modelo sin fila documentada queda pendiente, no adivinado.
    assert_eq!(
        zen.route_family("test"),
        opencode::OpenCodeRouteFamily::Undocumented
    );
    assert!(!zen.undocumented().is_empty());
}

#[test]
fn opencode_npm_package_has_no_immutable_commit_provenance() {
    let meta = read_fixture("opencode-cli-2.0.28.json").expect("metadata npm de 2.0.28");
    let parsed: serde_json::Value = serde_json::from_str(&meta).expect("metadata npm válida");

    assert_eq!(parsed["name"], opencode::OPENCODE_PACKAGE);
    assert_eq!(parsed["version"], opencode::OPENCODE_NPM_VERSION);
    assert!(
        parsed.get("gitHead").is_none(),
        "sin gitHead no existe procedencia inmutable paquete→commit"
    );
    assert_eq!(opencode::OPENCODE_SOURCE_TAG, "v2.0.28");
}

#[test]
fn opencode_tag_source_manifest_matches_pinned_version() {
    let pkg = read_fixture("opencode-v2.0.28/packages/cli/package.json")
        .expect("package.json del tag v2.0.28");
    let parsed: serde_json::Value = serde_json::from_str(&pkg).expect("package.json válido");
    assert_eq!(parsed["name"], opencode::OPENCODE_PACKAGE);
    assert_eq!(parsed["version"], opencode::OPENCODE_NPM_VERSION);
}
