use super::*;
use crate::client_contract::{ClientIdentity, IDENTITY_ORIGIN_OFFICIAL, SourceRef};

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct DemoPayload {
    version: String,
    models: Vec<String>,
}

/// Validator that accepts everything and defers every decision to the base.
struct BaseOnlyValidator;

impl ContractValidator<DemoPayload> for BaseOnlyValidator {
    fn rule_set_id(&self) -> &'static str {
        "demo-rules"
    }
    fn extractor_version(&self) -> u32 {
        1
    }
    fn validate(
        &self,
        _candidate: &Candidate<DemoPayload>,
        _previous: Option<&Snapshot<DemoPayload>>,
        _limits: &ContractLimits,
    ) -> Result<(), ContractError> {
        Ok(())
    }
}

fn candidate(version: &str) -> Candidate<DemoPayload> {
    Candidate::without_catalog(
        ContractId::Codex,
        ClientIdentity::versioned(version, IDENTITY_ORIGIN_OFFICIAL),
        "demo-rules",
        1,
        vec![SourceRef::pinned(
            "https://example.test/models.json",
            "rev-1",
            "d".repeat(64),
            super::super::EvidenceKind::PackageManifest,
        )],
        DemoPayload {
            version: version.to_string(),
            models: vec!["gpt-5".to_string()],
        },
    )
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "cc-store-{tag}-{}-{}",
        std::process::id(),
        hex_digest(tag.as_bytes()).get(..8).unwrap_or("x")
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn store(dir: &Path) -> ContractStore<DemoPayload> {
    ContractStore::new(
        ContractId::Codex,
        StoreConfig::new(dir).with_limits(ContractLimits::tight()),
        None,
        Arc::new(BaseOnlyValidator),
    )
}

#[test]
fn commit_persists_payload_and_manifest_then_restarts() {
    let dir = temp_dir("commit");
    let active = store(&dir);
    let published = active
        .commit(&candidate("1.0.0"))
        .expect("commit")
        .expect("activated");
    assert_eq!(
        published.schema_version,
        crate::client_contract::SCHEMA_VERSION
    );
    assert!(manifest_path(&dir, ContractId::Codex).exists());
    assert!(active.snapshot().is_some());

    // Restart: a fresh store over the same root, with no embedded seed.
    let restarted = store(&dir);
    let loaded = restarted.load().expect("restart load");
    assert_eq!(loaded.generation, published.generation);
    assert_eq!(loaded.payload.version, "1.0.0");
    assert_eq!(
        restarted.snapshot().expect("active").generation,
        loaded.generation
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn embedded_seed_activates_when_disk_is_empty() {
    let dir = temp_dir("seed");
    let seed = Snapshot::from_candidate(candidate("0.9.0")).expect("seed");
    let store = ContractStore::new(
        ContractId::Codex,
        StoreConfig::new(&dir).with_limits(ContractLimits::tight()),
        Some(seed),
        Arc::new(BaseOnlyValidator),
    );
    let loaded = store.load().expect("seed load");
    assert_eq!(loaded.client_identity.version.as_deref(), Some("0.9.0"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn no_snapshot_is_rejected_without_panicking() {
    let dir = temp_dir("empty");
    let store = store(&dir);
    let error = store.load().unwrap_err();
    assert!(matches!(error, ContractError::Rejected(_)));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_refused_persisted_state_falls_back_to_the_embedded_seed() {
    let dir = temp_dir("seed-fallback");
    let seed = Snapshot::from_candidate(candidate("0.9.0")).expect("seed");
    let writer = store(&dir);
    writer.commit(&candidate("1.0.0")).expect("first");
    // Corrupt the payload the manifest points at, so the restart finds a
    // persisted state it must refuse.
    let manifest = manifest_path(&dir, ContractId::Codex);
    let text = fs::read_to_string(&manifest).expect("manifest");
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("json");
    let generation = parsed["generation"].as_str().expect("generation");
    let payload = payload_path(&dir, ContractId::Codex, generation);
    fs::write(&payload, b"{\"version\":\"9.9.9\",\"models\":[]}").expect("corrupt");
    // The restarted store carries the seed and must serve it, not the
    // corrupted artifact.
    let restarted = ContractStore::new(
        ContractId::Codex,
        StoreConfig::new(&dir).with_limits(ContractLimits::tight()),
        Some(seed),
        Arc::new(BaseOnlyValidator),
    );
    let loaded = restarted.load().expect("seed fallback");
    assert_eq!(
        loaded.client_identity.version.as_deref(),
        Some("0.9.0"),
        "a refused persisted state must degrade to the seed"
    );
    assert!(
        loaded.payload_digest().is_ok(),
        "the served seed must be re-validated, not raw"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn identical_candidate_does_not_republish() {
    let dir = temp_dir("same");
    let store = store(&dir);
    let first = store.commit(&candidate("1.0.0")).expect("first");
    assert!(first.is_some());
    let second = store.commit(&candidate("1.0.0")).expect("second");
    assert!(second.is_none(), "identical content must not republish");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn same_version_with_changed_content_is_a_conflict() {
    let dir = temp_dir("conflict");
    let store = store(&dir);
    store.commit(&candidate("1.0.0")).expect("first");

    let mut drifted = candidate("1.0.0");
    drifted.payload.models = vec!["gpt-5".to_string(), "gpt-5-mini".to_string()];
    let error = store.commit(&drifted).unwrap_err();
    assert!(
        matches!(error, ContractError::VersionConflict { .. }),
        "same version with different content must conflict, got {error:?}"
    );
    assert_eq!(
        store.snapshot().expect("previous survives").payload.models,
        vec!["gpt-5".to_string()]
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn downgrade_is_rejected_by_semver() {
    let dir = temp_dir("downgrade");
    let store = store(&dir);
    store.commit(&candidate("2.0.0")).expect("first");
    let error = store.commit(&candidate("1.9.0")).unwrap_err();
    assert!(matches!(error, ContractError::Downgrade { .. }));
    assert_eq!(store.snapshot().expect("active").payload.version, "2.0.0");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn validator_rejection_leaves_no_artifact() {
    let dir = temp_dir("rejected");
    struct Rejecting;
    impl ContractValidator<DemoPayload> for Rejecting {
        fn rule_set_id(&self) -> &'static str {
            "demo-rules"
        }
        fn extractor_version(&self) -> u32 {
            1
        }
        fn validate(
            &self,
            _candidate: &Candidate<DemoPayload>,
            _previous: Option<&Snapshot<DemoPayload>>,
            _limits: &ContractLimits,
        ) -> Result<(), ContractError> {
            Err(ContractError::Rejected("provider guard".to_string()))
        }
    }
    let store = ContractStore::new(
        ContractId::Codex,
        StoreConfig::new(&dir).with_limits(ContractLimits::tight()),
        None,
        Arc::new(Rejecting),
    );
    assert!(store.commit(&candidate("1.0.0")).is_err());
    assert!(!manifest_path(&dir, ContractId::Codex).exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn oversized_payload_is_refused_before_any_write() {
    let dir = temp_dir("oversize");
    let store = store(&dir);
    let mut big = candidate("1.0.0");
    big.payload.models = vec!["x".repeat(1024); 32];
    let error = store.commit(&big).unwrap_err();
    assert!(matches!(error, ContractError::PayloadTooLarge { .. }));
    assert!(!dir.exists(), "no artifact may be created");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn io_failure_during_commit_keeps_the_previous_snapshot() {
    let dir = temp_dir("io");
    fs::create_dir_all(&dir).expect("mkdir");
    let store = store(&dir);
    store.commit(&candidate("1.0.0")).expect("first");

    // A file at the root path forces an IO error even when running as root.
    let saved = dir.with_extension("saved");
    fs::rename(&dir, &saved).expect("move root");
    fs::write(&dir, b"not a directory").expect("block root");
    let error = store.commit(&candidate("2.0.0"));
    fs::remove_file(&dir).expect("remove blocker");
    fs::rename(&saved, &dir).expect("restore root");
    assert!(matches!(error, Err(ContractError::Io(_))), "got {error:?}");
    assert_eq!(
        store
            .snapshot()
            .expect("previous still active")
            .payload
            .version,
        "1.0.0"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn tampered_manifest_is_refused_on_restart() {
    let dir = temp_dir("tamper");
    let store = store(&dir);
    store.commit(&candidate("1.0.0")).expect("first");
    let manifest = manifest_path(&dir, ContractId::Codex);
    let mut text = fs::read_to_string(&manifest).expect("manifest");
    let at = text.find("\"payload\"").expect("payload field");
    text.replace_range(at..at + 9, "\"payloadX");
    fs::write(&manifest, text).expect("tamper");
    let error = store.load().unwrap_err();
    assert!(!error.to_string().is_empty());
    assert!(
        store
            .snapshot()
            .expect("previous validated snapshot survives")
            .payload
            .version
            == "1.0.0",
        "a refused reload must preserve the last valid snapshot"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn missing_payload_file_is_refused_even_with_a_valid_manifest() {
    let dir = temp_dir("orphan");
    let store = store(&dir);
    let published = store
        .commit(&candidate("1.0.0"))
        .expect("first")
        .expect("active");
    let payload = payload_path(&dir, ContractId::Codex, &published.generation);
    fs::remove_file(&payload).expect("remove payload");
    let error = store.load().unwrap_err();
    assert!(
        matches!(error, ContractError::Io(_) | ContractError::Encoding(_)),
        "got {error:?}"
    );
    assert!(
        store
            .snapshot()
            .expect("previous validated snapshot survives")
            .payload
            .version
            == "1.0.0",
        "a refused reload must preserve the last valid snapshot"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn generation_must_be_a_safe_artifact_name() {
    assert!(is_safe_generation(&"a".repeat(64)));
    assert!(!is_safe_generation(""));
    assert!(!is_safe_generation("../../etc/passwd"));
    assert!(!is_safe_generation("with space"));
}

#[test]
fn refresh_report_shape_is_constructible() {
    let dir = temp_dir("report");
    let store = store(&dir);
    let outcome = store.commit(&candidate("1.0.0")).expect("commit");
    let report = match outcome {
        Some(snapshot) => RefreshReport::Activated {
            generation: snapshot.generation.clone(),
            snapshot,
        },
        None => RefreshReport::Unchanged {
            generation: "none".to_string(),
        },
    };
    assert!(matches!(report, RefreshReport::Activated { .. }));
    let _ = fs::remove_dir_all(&dir);
}
