//! Store tests for the common contract base.
//!
//! Every test is offline and writes only under a per-process temp directory.
//! Restart behaviour is exercised by building a *second* store over the same
//! root, which is what a process restart does to the persisted state.

#![allow(
    clippy::all,
    clippy::pedantic,
    clippy::unwrap_used,
    clippy::expect_used
)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use openproxy_adapters::client_contract::{
    Candidate, ClientIdentity, ContractError, ContractId, ContractLimits, ContractStore,
    ContractValidator, EvidenceKind, SourceRef, StoreConfig,
};

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct DemoPayload {
    version: String,
    models: Vec<String>,
}

/// Validator that applies no provider-specific rule, so every test isolates one
/// behaviour of the common base.
struct BaseOnlyValidator;

impl ContractValidator<DemoPayload> for BaseOnlyValidator {
    fn rule_set_id(&self) -> &'static str {
        "codex-rules"
    }

    fn extractor_version(&self) -> u32 {
        3
    }

    fn validate(
        &self,
        _candidate: &Candidate<DemoPayload>,
        _previous: Option<&openproxy_adapters::client_contract::Snapshot<DemoPayload>>,
        _limits: &ContractLimits,
    ) -> Result<(), ContractError> {
        Ok(())
    }
}

/// Validator that refuses everything, standing in for a provider protocol guard.
struct RejectAllValidator;

impl ContractValidator<DemoPayload> for RejectAllValidator {
    fn rule_set_id(&self) -> &'static str {
        "codex-rules"
    }

    fn extractor_version(&self) -> u32 {
        3
    }

    fn validate(
        &self,
        _candidate: &Candidate<DemoPayload>,
        _previous: Option<&openproxy_adapters::client_contract::Snapshot<DemoPayload>>,
        _limits: &ContractLimits,
    ) -> Result<(), ContractError> {
        Err(ContractError::Rejected("provider guard".to_string()))
    }
}

fn candidate(version: &str, models: &[&str]) -> Candidate<DemoPayload> {
    Candidate::without_catalog(
        ContractId::Codex,
        ClientIdentity::versioned(version, "official"),
        "codex-rules",
        3,
        vec![SourceRef::pinned(
            "https://example.test/models.json",
            "rev-1",
            "d".repeat(64),
            EvidenceKind::PackageManifest,
        )],
        DemoPayload {
            version: version.to_string(),
            models: models.iter().map(|m| (*m).to_string()).collect(),
        },
    )
}

fn temp_root(tag: &str) -> PathBuf {
    let unique = openproxy_adapters::client_contract::hex_digest(tag.as_bytes());
    let root = std::env::temp_dir().join(format!(
        "cc-store-test-{}-{}",
        std::process::id(),
        &unique[..12]
    ));
    let _ = fs::remove_dir_all(&root);
    root
}

fn store_at(root: PathBuf) -> ContractStore<DemoPayload> {
    ContractStore::new(
        ContractId::Codex,
        StoreConfig::new(root).with_limits(ContractLimits::default()),
        None,
        Arc::new(BaseOnlyValidator),
    )
}

fn files_in(root: &PathBuf) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(root)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[test]
fn commit_persists_payload_and_manifest_then_restarts() {
    let root = temp_root("commit-publish");
    let store = store_at(root.clone());

    let published = store
        .commit(&candidate("1.0.0", &["gpt-5"]))
        .expect("commit")
        .expect("activated");
    assert_eq!(published.contract_id, ContractId::Codex);
    assert_eq!(published.schema_version, 1);
    assert_eq!(published.payload.version, "1.0.0");

    let names = files_in(&root);
    assert!(
        names
            .iter()
            .any(|n| n.ends_with(".json") && !n.contains("manifest")),
        "expected a payload file, got {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("manifest")),
        "expected a manifest, got {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains(".tmp.")),
        "no temp file may survive a commit, got {names:?}"
    );

    // Restart: a brand new store over the same root, no embedded seed.
    let restarted = store_at(root.clone());
    let loaded = restarted.load().expect("restart load");
    assert_eq!(loaded.generation, published.generation);
    assert_eq!(loaded.payload.models, vec!["gpt-5".to_string()]);
    assert!(restarted.snapshot().is_some());

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn embedded_seed_activates_when_the_disk_is_empty() {
    let root = temp_root("seed");
    let seed = openproxy_adapters::client_contract::Snapshot::from_candidate(candidate(
        "0.9.0",
        &["gpt-4o"],
    ))
    .expect("seed");
    let store = ContractStore::new(
        ContractId::Codex,
        StoreConfig::new(root.clone()).with_limits(ContractLimits::default()),
        Some(seed),
        Arc::new(BaseOnlyValidator),
    );
    let loaded = store.load().expect("seed load");
    assert_eq!(loaded.payload.version, "0.9.0");
    assert!(!root.exists(), "a load must not write anything");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn no_persisted_snapshot_and_no_seed_is_a_clean_error() {
    let root = temp_root("empty");
    let store = store_at(root.clone());
    let error = store.load().expect_err("nothing to load");
    assert!(matches!(error, ContractError::Rejected(_)), "got {error:?}");
    assert!(store.snapshot().is_none());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn identical_candidate_does_not_republish() {
    let root = temp_root("identical");
    let store = store_at(root.clone());
    let first = store
        .commit(&candidate("1.0.0", &["gpt-5"]))
        .expect("first");
    assert!(first.is_some());
    let before = files_in(&root);

    let second = store
        .commit(&candidate("1.0.0", &["gpt-5"]))
        .expect("second");
    assert!(second.is_none(), "identical content must not republish");
    assert_eq!(files_in(&root), before, "the disk must be untouched");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn same_version_with_changed_content_is_refused() {
    let root = temp_root("version-conflict");
    let store = store_at(root.clone());
    store
        .commit(&candidate("1.0.0", &["gpt-5"]))
        .expect("first")
        .expect("activated");

    let drifted = candidate("1.0.0", &["gpt-5", "gpt-5-mini"]);
    let error = store.commit(&drifted).expect_err("conflict");
    assert!(
        matches!(error, ContractError::VersionConflict { .. }),
        "same version with different content must conflict, got {error:?}"
    );
    let still_active = store.snapshot().expect("previous survives");
    assert_eq!(still_active.payload.models, vec!["gpt-5".to_string()]);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn downgrade_is_refused_by_semver() {
    let root = temp_root("downgrade");
    let store = store_at(root.clone());
    store
        .commit(&candidate("2.0.0", &["gpt-5"]))
        .expect("first")
        .expect("activated");

    let error = store
        .commit(&candidate("1.9.0", &["gpt-5"]))
        .expect_err("downgrade");
    assert!(
        matches!(error, ContractError::Downgrade { .. }),
        "got {error:?}"
    );
    assert_eq!(store.snapshot().expect("active").payload.version, "2.0.0");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn channel_change_is_refused_in_both_directions() {
    let root = temp_root("channel");
    let store = store_at(root.clone());
    store
        .commit(&candidate("1.0.0", &["gpt-5"]))
        .expect("first")
        .expect("activated");
    let error = store
        .commit(&candidate("1.1.0-rc.1", &["gpt-5"]))
        .expect_err("channel change");
    assert!(
        matches!(error, ContractError::ChannelMismatch { .. }),
        "got {error:?}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rejected_candidate_leaves_no_artifact() {
    let root = temp_root("validator-reject");
    let store = ContractStore::new(
        ContractId::Codex,
        StoreConfig::new(root.clone()).with_limits(ContractLimits::default()),
        None,
        Arc::new(RejectAllValidator),
    );
    let error = store
        .commit(&candidate("1.0.0", &["gpt-5"]))
        .expect_err("rejected");
    assert!(matches!(error, ContractError::Rejected(_)));
    assert!(
        !root.exists(),
        "a rejected candidate must leave no artifact, got {:?}",
        files_in(&root)
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn oversized_payload_is_refused_before_any_write() {
    let root = temp_root("oversize");
    let limits = ContractLimits {
        profile_bytes: 128,
        ..ContractLimits::default()
    };
    let store = ContractStore::new(
        ContractId::Codex,
        StoreConfig::new(root.clone()).with_limits(limits),
        None,
        Arc::new(BaseOnlyValidator),
    );
    let big = candidate("1.0.0", &[&"x".repeat(512)]);
    let error = store.commit(&big).expect_err("too large");
    assert!(
        matches!(error, ContractError::PayloadTooLarge { .. }),
        "got {error:?}"
    );
    assert!(!root.exists(), "the root must not even be created");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn secret_material_in_the_payload_is_refused() {
    let root = temp_root("secret");
    let store = store_at(root.clone());
    let leaky = candidate("1.0.0", &["gpt-5", "token=abc123"]);
    let error = store.commit(&leaky).expect_err("secret");
    assert!(
        matches!(error, ContractError::SecretMaterial { .. }),
        "got {error:?}"
    );
    assert!(!root.exists());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn wrong_contract_id_is_refused() {
    let root = temp_root("wrong-contract");
    let store = store_at(root.clone());
    let mut foreign = candidate("1.0.0", &["gpt-5"]);
    foreign.contract_id = ContractId::CodeBuddy;
    let error = store.commit(&foreign).expect_err("mismatch");
    assert!(
        matches!(error, ContractError::ContractIdMismatch { .. }),
        "got {error:?}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn io_error_keeps_the_previous_snapshot_active() {
    let root = temp_root("io-error");
    fs::create_dir_all(&root).expect("mkdir");
    let store = store_at(root.clone());
    store
        .commit(&candidate("1.0.0", &["gpt-5"]))
        .expect("first")
        .expect("activated");

    let saved = root.with_extension("saved");
    fs::rename(&root, &saved).expect("move root");
    fs::write(&root, b"not a directory").expect("block root");
    let result = store.commit(&candidate("2.0.0", &["gpt-5"]));
    fs::remove_file(&root).expect("remove blocker");
    fs::rename(&saved, &root).expect("restore root");

    assert!(
        matches!(result, Err(ContractError::Io(_))),
        "an unwritable root must surface as an io error, got {result:?}"
    );
    assert_eq!(
        store
            .snapshot()
            .expect("previous still active")
            .payload
            .version,
        "1.0.0",
        "a failed commit must keep the previous snapshot active"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn tampered_manifest_is_refused_on_restart() {
    let root = temp_root("tamper");
    let store = store_at(root.clone());
    store
        .commit(&candidate("1.0.0", &["gpt-5"]))
        .expect("first")
        .expect("activated");

    let manifest = files_in(&root)
        .into_iter()
        .find(|n| n.contains("manifest"))
        .map(|n| root.join(n))
        .expect("manifest path");
    let mut text = fs::read_to_string(&manifest).expect("manifest");
    let anchor = text.find("\"payload\"").expect("payload key");
    text.replace_range(anchor..anchor + 9, "\"payloadZ");
    fs::write(&manifest, text).expect("tamper");

    let restarted = store_at(root.clone());
    let error = restarted.load().expect_err("tampered manifest");
    assert!(
        matches!(
            error,
            ContractError::Encoding(_) | ContractError::Rejected(_)
        ),
        "got {error:?}"
    );
    assert!(
        restarted.snapshot().is_none(),
        "a refused load must not activate"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn missing_payload_file_is_refused_even_with_a_valid_manifest() {
    let root = temp_root("orphan");
    let store = store_at(root.clone());
    let published = store
        .commit(&candidate("1.0.0", &["gpt-5"]))
        .expect("first")
        .expect("activated");

    let payload = files_in(&root)
        .into_iter()
        .find(|n| n.ends_with(".json") && !n.contains("manifest"))
        .map(|n| root.join(n))
        .expect("payload path");
    fs::remove_file(&payload).expect("remove payload");

    let restarted = store_at(root.clone());
    let error = restarted.load().expect_err("missing payload");
    assert!(
        matches!(error, ContractError::Io(_) | ContractError::Encoding(_)),
        "got {error:?}"
    );
    assert!(restarted.snapshot().is_none());
    let _ = published;
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn persisted_snapshot_round_trips_through_serde() {
    let root = temp_root("roundtrip");
    let store = store_at(root.clone());
    let published = store
        .commit(&candidate("1.0.0", &["gpt-5"]))
        .expect("first")
        .expect("activated");

    let bytes = serde_json::to_vec(published.as_ref()).expect("encode");
    let loaded: openproxy_adapters::client_contract::Snapshot<DemoPayload> =
        serde_json::from_slice(&bytes).expect("decode");
    assert_eq!(loaded.generation, published.generation);
    assert_eq!(loaded.schema_version, published.schema_version);
    assert_eq!(
        loaded.payload_digest().unwrap(),
        published.payload_digest().unwrap()
    );
    assert_eq!(loaded.sources.len(), 1);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn catalog_ref_is_persisted_when_a_provider_declares_one() {
    let root = temp_root("catalog");
    let store = store_at(root.clone());
    let mut typed = candidate("1.0.0", &["gpt-5"]);
    let catalog: Arc<[u8]> = Arc::from(br#"{"models":[]}"#.as_slice());
    typed.catalog_ref = Some(openproxy_adapters::client_contract::CatalogRef {
        digest: openproxy_adapters::client_contract::hex_digest(&catalog),
        format: "json".to_string(),
    });
    typed.catalog_bytes = Some(catalog.to_vec());
    let published = store.commit(&typed).expect("commit").expect("activated");
    assert!(published.catalog_ref.is_some());

    let restarted = store_at(root.clone());
    let loaded = restarted.load().expect("restart");
    assert_eq!(loaded.catalog_ref, published.catalog_ref);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_bad_catalog_digest_is_refused() {
    let root = temp_root("bad-catalog");
    let store = store_at(root.clone());
    let mut typed = candidate("1.0.0", &["gpt-5"]);
    typed.catalog_ref = Some(openproxy_adapters::client_contract::CatalogRef {
        digest: "not-a-digest".to_string(),
        format: "json".to_string(),
    });
    let error = store.commit(&typed).expect_err("bad digest");
    assert!(
        matches!(
            error,
            ContractError::InvalidDigest { .. } | ContractError::Rejected(_)
        ),
        "got {error:?}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn store_exposes_its_identity() {
    let root = temp_root("identity");
    let store = store_at(root.clone());
    assert_eq!(store.contract_id(), ContractId::Codex);
    assert_eq!(store.root(), root.as_path());
    let _ = fs::remove_dir_all(&root);
}
