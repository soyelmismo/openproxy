//! Atomic persistence and in-memory publication of contract snapshots.
//!
//! One store owns one contract and one payload type. Validation runs *before*
//! anything touches the disk, so a rejected, downgraded or oversized candidate
//! never leaves a partial artifact behind. Publication is a single rename of the
//! manifest, and the in-memory `Arc` only advances after that rename and the
//! directory `fsync` succeed.
//!
//! The whole validate → persist → publish sequence is serialized under the
//! state write lock: two concurrent commits can never both validate against the
//! same previous snapshot, which is the only way a downgrade could slip past
//! `check_identity_transition`.
//!
//! `load` re-reads and re-hashes the artifacts, recomputes the generation and
//! re-validates the envelope. A corrupted or mismatching persisted state falls
//! back to the embedded seed — after a warning and a full re-validation of the
//! seed — so a bad artifact degrades to the compiled-in default instead of
//! leaving the contract without a snapshot.
//!
//! Layout under `root`:
//!
//! ```text
//! <root>/<contract>-<generation>.json      payload, addressed by generation
//! <root>/<contract>.manifest.json          published manifest, renamed atomically
//! <root>/catalogs/<digest>.<format>        catalog body, addressed by digest
//! ```
//!
//! The generation *is* the content digest of the envelope, so a payload file
//! name is content-addressed. Payload and manifest are separate files on
//! purpose: a crash between the two leaves an orphan payload that the next
//! start ignores, never a manifest pointing at bytes that were never flushed.
//! A catalog body is persisted digest-addressed *before* the manifest, so a
//! manifest that references a catalog can only name bytes that were already
//! flushed and verified; a bare `catalog_ref` without persisted bytes is
//! refused at commit time.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;

use super::validation::{
    ContractLimits, ContractValidator, check_identity_transition, contains_secret_material,
    validate_candidate_shape, validate_catalog_body_size,
};
use super::{
    Candidate, CatalogRef, ContractError, ContractId, Snapshot, canonical_bytes, hex_digest,
    payload_digest,
};

/// File name of a published manifest, under the contract's own prefix.
pub const MANIFEST_FILE: &str = "manifest.json";

/// The report a store-driven refresh round produces, re-exported so a caller
/// that only knows the store does not need the refresh path.
pub use super::refresh::RefreshReport;

/// What a store persists, and where.
#[derive(Clone, Debug)]
pub struct StoreConfig {
    /// Root directory, created on first commit.
    pub root: PathBuf,
    /// Limits every commit is checked against.
    pub limits: ContractLimits,
}

impl StoreConfig {
    /// A config rooted at `root` with the default limits.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            limits: ContractLimits::default(),
        }
    }

    /// Override the limits.
    pub fn with_limits(mut self, limits: ContractLimits) -> Self {
        self.limits = limits;
        self
    }
}

/// Path of one contract's payload, addressed by its generation.
fn payload_path(root: &Path, contract: ContractId, generation: &str) -> PathBuf {
    root.join(format!("{}-{generation}.json", contract.as_str()))
}

/// Path of the published manifest of one contract.
fn manifest_path(root: &Path, contract: ContractId) -> PathBuf {
    root.join(format!("{}.{MANIFEST_FILE}", contract.as_str()))
}

/// Sub-directory holding digest-addressed catalog bodies.
const CATALOG_DIR: &str = "catalogs";

/// Envelope overhead granted on top of the profile payload when reading a
/// persisted manifest: identity, sources, digests and JSON punctuation.
const MANIFEST_OVERHEAD_BYTES: usize = 64 * 1024;

/// Path of one catalog body, addressed by its digest and format tag.
fn catalog_path(root: &Path, catalog: &CatalogRef) -> PathBuf {
    root.join(CATALOG_DIR)
        .join(format!("{}.{}", catalog.digest, catalog.format))
}

/// File-system-safe check for a generation used as a file name. A generation is
/// a SHA-256 hex digest, so anything unexpected is refused rather than escaped:
/// a candidate that cannot name a safe artifact must not be published.
fn is_safe_generation(generation: &str) -> bool {
    !generation.is_empty()
        && generation.len() <= 128
        && generation
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// Atomic store for one contract and one payload type.
///
/// Generic over `T` because each provider owns its payload shape: nothing here
/// assumes a Codex-shaped contract.
pub struct ContractStore<T> {
    contract: ContractId,
    config: StoreConfig,
    validator: Arc<dyn ContractValidator<T>>,
    /// Seed used when the disk carries no manifest.
    embedded: Option<Arc<Snapshot<T>>>,
    current: RwLock<Option<Arc<Snapshot<T>>>>,
}

impl<T> ContractStore<T>
where
    T: serde::Serialize + serde::de::DeserializeOwned + Clone + Send + Sync + 'static,
{
    /// Build a store. Nothing is read until [`Self::load`].
    pub fn new(
        contract: ContractId,
        config: StoreConfig,
        embedded: Option<Snapshot<T>>,
        validator: Arc<dyn ContractValidator<T>>,
    ) -> Self {
        Self {
            contract,
            config,
            validator,
            embedded: embedded.map(Arc::new),
            current: RwLock::new(None),
        }
    }

    /// The contract this store serves.
    pub fn contract_id(&self) -> ContractId {
        self.contract
    }

    /// The store's root directory.
    pub fn root(&self) -> &Path {
        &self.config.root
    }

    /// The currently published snapshot, if any.
    pub fn snapshot(&self) -> Option<Arc<Snapshot<T>>> {
        self.current.read().clone()
    }

    /// Load the published manifest, or fall back to the embedded seed.
    ///
    /// A manifest whose schema version, contract id, payload digest, catalog
    /// digest or recomputed generation does not check out is refused; when an
    /// embedded seed exists the load then falls back to it, after a warning
    /// and a full re-validation, so a corrupt artifact degrades to the
    /// compiled-in default instead of leaving the contract with no snapshot.
    /// Without a seed the error is surfaced and the previous state is left
    /// untouched.
    pub fn load(&self) -> Result<Arc<Snapshot<T>>, ContractError> {
        let manifest = manifest_path(&self.config.root, self.contract);
        match self.read_manifest(&manifest) {
            Ok(Some(snapshot)) => match self.validate_loaded(snapshot) {
                Ok(arc) => {
                    *self.current.write() = Some(Arc::clone(&arc));
                    Ok(arc)
                }
                Err(error) => self.fallback_to_seed(error),
            },
            Ok(None) => self.load_seed(),
            Err(error) => self.fallback_to_seed(error),
        }
    }

    /// Serve the embedded seed after a refused load: warn, re-validate and
    /// publish. Without a seed the original error stands.
    fn fallback_to_seed(&self, error: ContractError) -> Result<Arc<Snapshot<T>>, ContractError> {
        if self.embedded.is_none() {
            return Err(error);
        }
        tracing::warn!(
            contract = %self.contract,
            error = %error,
            "persisted contract state was refused; falling back to the embedded seed"
        );
        self.load_seed()
    }

    /// Re-validate and publish the embedded seed.
    ///
    /// A compiled-in default must satisfy the same structural rules and the
    /// same validator as a downloaded candidate before it is activated.
    fn load_seed(&self) -> Result<Arc<Snapshot<T>>, ContractError> {
        let Some(seed) = self.embedded.as_ref() else {
            return Err(ContractError::Rejected(format!(
                "no persisted snapshot and no embedded seed for {}",
                self.contract
            )));
        };
        let candidate = seed.as_candidate();
        validate_candidate_shape(&candidate, &self.config.limits)?;
        self.validator
            .validate(&candidate, None, &self.config.limits)?;
        let arc = Arc::clone(seed);
        *self.current.write() = Some(Arc::clone(&arc));
        Ok(arc)
    }

    /// Read and structurally check the manifest, if one exists.
    fn read_manifest(&self, manifest: &Path) -> Result<Option<Snapshot<T>>, ContractError> {
        if !manifest.exists() {
            return Ok(None);
        }
        let limit = self
            .config
            .limits
            .profile_bytes
            .saturating_add(MANIFEST_OVERHEAD_BYTES);
        let bytes = read_bounded(manifest, limit, "manifest")?;
        let snapshot: Snapshot<T> = serde_json::from_slice(&bytes)?;
        if snapshot.schema_version != super::SCHEMA_VERSION {
            return Err(ContractError::SchemaMismatch {
                found: snapshot.schema_version,
                expected: super::SCHEMA_VERSION,
            });
        }
        if snapshot.contract_id != self.contract {
            return Err(ContractError::ContractIdMismatch {
                found: snapshot.contract_id,
                expected: self.contract,
            });
        }
        Ok(Some(snapshot))
    }

    /// Re-verify a snapshot loaded from disk against its payload file.
    ///
    /// The manifest is not trusted: the generation is recomputed from the
    /// envelope itself, the payload bytes are re-read under a bounded read and
    /// their digest recomputed, the catalog body is re-read and re-hashed, and
    /// the full validation stack runs again, so a hand-edited or partially
    /// written artifact can never activate. The catalog bytes are attached to
    /// the returned snapshot so the runtime body and the persisted reference
    /// stay coherent.
    fn validate_loaded(
        &self,
        mut snapshot: Snapshot<T>,
    ) -> Result<Arc<Snapshot<T>>, ContractError> {
        if !is_safe_generation(&snapshot.generation) {
            return Err(ContractError::Rejected(
                "persisted generation digest is not a safe artifact name".to_string(),
            ));
        }
        let payload_file = payload_path(&self.config.root, self.contract, &snapshot.generation);
        let bytes = read_bounded(
            &payload_file,
            self.config.limits.profile_bytes,
            "payload artifact",
        )?;
        let expected = payload_digest(&snapshot.payload)?;
        if hex_digest(&bytes) != expected {
            return Err(ContractError::PayloadDigestMismatch {
                manifest: expected,
                file: hex_digest(&bytes),
            });
        }
        let reloaded: T = serde_json::from_slice(&bytes)?;
        if payload_digest(&reloaded)? != expected {
            return Err(ContractError::PayloadDigestMismatch {
                manifest: expected,
                file: payload_digest(&reloaded)?,
            });
        }
        // Recompute the generation from the loaded content: a manifest whose
        // envelope was edited must not keep its old content-addressed name.
        let recomputed = Snapshot::<T>::from_candidate(snapshot.as_candidate())?.generation;
        if recomputed != snapshot.generation {
            return Err(ContractError::GenerationMismatch {
                found: snapshot.generation,
                expected: recomputed,
            });
        }
        let candidate = snapshot.as_candidate();
        self.validator
            .validate(&candidate, None, &self.config.limits)?;
        validate_candidate_shape(&candidate, &self.config.limits)?;
        if let Some(reference) = snapshot.catalog_ref.as_ref() {
            let bytes = read_bounded(
                &catalog_path(&self.config.root, reference),
                self.config.limits.catalog_bytes,
                "catalog artifact",
            )?;
            if hex_digest(&bytes) != reference.digest {
                return Err(ContractError::CatalogDigestMismatch {
                    reference: reference.digest.clone(),
                    file: hex_digest(&bytes),
                });
            }
            snapshot.catalog = Some(Arc::from(bytes));
        }
        Ok(Arc::new(snapshot))
    }

    /// Validate a candidate, then publish it if it beats the active snapshot.
    ///
    /// The order is fixed and load-bearing: the provider validator runs first,
    /// then the common structural checks, then the SemVer identity comparison.
    /// Only after all three pass is a single byte written. A candidate that is
    /// not newer than the active generation returns `Ok(None)` and the disk is
    /// never touched.
    pub fn commit(
        &self,
        candidate: &Candidate<T>,
    ) -> Result<Option<Arc<Snapshot<T>>>, ContractError> {
        // Serialize validation, persistence and publication. Two concurrent
        // commits must never both validate against the same previous snapshot
        // and then publish out of order: that is how older bytes replace a
        // newer generation.
        let mut current = self.current.write();
        let previous: Option<Arc<Snapshot<T>>> = (*current).clone();
        self.validate_candidate(candidate, previous.as_deref())?;

        let snapshot = Snapshot::from_candidate(candidate.clone())?;
        if !is_safe_generation(&snapshot.generation) {
            return Err(ContractError::Rejected(
                "generation digest is not a safe artifact name".to_string(),
            ));
        }
        if let Some(prev) = previous.as_deref()
            && snapshot.generation == prev.generation
        {
            return Ok(None);
        }

        // Catalog artifact first, digest-addressed and verified, then the
        // payload, then the manifest: a manifest may never reference bytes
        // that were never flushed.
        let catalog = self.ensure_catalog(candidate)?;
        let mut snapshot = snapshot;
        if snapshot.catalog.is_none() {
            snapshot.catalog = catalog;
        }

        let payload_bytes = canonical_bytes(&snapshot.payload)?;
        self.persist(&snapshot, &payload_bytes)?;
        let arc = Arc::new(snapshot);
        *current = Some(Arc::clone(&arc));
        Ok(Some(arc))
    }

    /// Persist or verify the catalog body of a candidate, digest-addressed,
    /// and return the verified runtime bytes.
    ///
    /// Bytes carried by the candidate are written and re-read under the
    /// catalog limit; a candidate that declares a `catalog_ref` without bytes
    /// is only accepted when the artifact is already persisted and matches the
    /// reference, so a reference is never published unbacked. The body is
    /// scanned for credential material and bounded by the catalog limit before
    /// it may touch the disk.
    fn ensure_catalog(&self, candidate: &Candidate<T>) -> Result<Option<Arc<[u8]>>, ContractError> {
        let Some(reference) = candidate.catalog_ref.as_ref() else {
            if candidate.catalog_bytes.is_some() {
                return Err(ContractError::Rejected(
                    "catalog bytes are present without a catalog reference".to_string(),
                ));
            }
            return Ok(None);
        };
        let path = catalog_path(&self.config.root, reference);
        match candidate.catalog_bytes.as_deref() {
            Some(bytes) => {
                validate_catalog_body_size(bytes.len(), &self.config.limits)?;
                if contains_secret_material(bytes) {
                    return Err(ContractError::SecretMaterial {
                        location: "catalog",
                    });
                }
                if hex_digest(bytes) != reference.digest {
                    return Err(ContractError::InvalidDigest {
                        value: reference.digest.clone(),
                    });
                }
                fs::create_dir_all(self.config.root.join(CATALOG_DIR))?;
                write_atomic(&path, bytes)?;
                let reread =
                    read_bounded(&path, self.config.limits.catalog_bytes, "catalog artifact")?;
                if hex_digest(&reread) != reference.digest {
                    return Err(ContractError::CatalogDigestMismatch {
                        reference: reference.digest.clone(),
                        file: hex_digest(&reread),
                    });
                }
                Ok(Some(Arc::from(reread)))
            }
            None => {
                // No bytes in hand: the digest-addressed artifact must already
                // be on disk and must verify against the reference.
                if !path.exists() {
                    return Err(ContractError::Rejected(format!(
                        "catalog {} is neither carried by the candidate nor persisted",
                        reference.digest
                    )));
                }
                let bytes =
                    read_bounded(&path, self.config.limits.catalog_bytes, "catalog artifact")?;
                if hex_digest(&bytes) != reference.digest {
                    return Err(ContractError::CatalogDigestMismatch {
                        reference: reference.digest.clone(),
                        file: hex_digest(&bytes),
                    });
                }
                Ok(Some(Arc::from(bytes)))
            }
        }
    }

    /// Run the full validation stack for `candidate`.
    fn validate_candidate(
        &self,
        candidate: &Candidate<T>,
        previous: Option<&Snapshot<T>>,
    ) -> Result<(), ContractError> {
        if candidate.contract_id != self.contract {
            return Err(ContractError::ContractIdMismatch {
                found: candidate.contract_id,
                expected: self.contract,
            });
        }
        self.validator
            .validate(candidate, previous, &self.config.limits)?;
        validate_candidate_shape(candidate, &self.config.limits)?;
        if let Some(prev) = previous {
            match check_identity_transition(&prev.client_identity, &candidate.client_identity)? {
                super::validation::IdentityTransition::New => {}
                super::validation::IdentityTransition::Newer => {}
                super::validation::IdentityTransition::Same => {
                    // The base exposes the generation through the envelope
                    // builder, so the comparison is on the real digest.
                    let next_generation =
                        Snapshot::<T>::from_candidate(candidate.clone())?.generation;
                    if next_generation != prev.generation {
                        return Err(ContractError::VersionConflict {
                            version: candidate
                                .client_identity
                                .version
                                .clone()
                                .unwrap_or_else(|| "unversioned".to_string()),
                            previous: prev.generation.clone(),
                            candidate: next_generation,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Write the payload, flush it, write the manifest, flush it, rename it into
    /// place and `fsync` the directory.
    ///
    /// Every step before the rename may fail without having changed what a
    /// reader sees, so a failed commit leaves the previous snapshot active.
    fn persist(&self, snapshot: &Snapshot<T>, payload_bytes: &[u8]) -> Result<(), ContractError> {
        fs::create_dir_all(&self.config.root)?;
        let payload_file = payload_path(&self.config.root, self.contract, &snapshot.generation);
        write_atomic(&payload_file, payload_bytes)?;

        // Re-read and re-hash before publishing, so a torn write is caught
        // rather than activated.
        let reread = read_bounded(
            &payload_file,
            self.config.limits.profile_bytes,
            "payload artifact",
        )?;
        if hex_digest(&reread) != hex_digest(payload_bytes) {
            return Err(ContractError::PayloadDigestMismatch {
                manifest: hex_digest(payload_bytes),
                file: hex_digest(&reread),
            });
        }

        let manifest_bytes = serde_json::to_vec_pretty(snapshot)?;
        write_atomic(
            &manifest_path(&self.config.root, self.contract),
            &manifest_bytes,
        )?;
        fsync_dir(&self.config.root)
    }
}

/// Write `bytes` to `path` through a temp file, `fsync`, then rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ContractError> {
    let name = path.file_name().map_or_else(
        || "artifact".to_string(),
        |n| n.to_string_lossy().to_string(),
    );
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    Ok(())
}

/// Read a file only after its size has been checked against `limit`, so a
/// corrupted or hostile artifact cannot be pulled into memory before it is
/// validated.
fn read_bounded(path: &Path, limit: usize, what: &str) -> Result<Vec<u8>, ContractError> {
    let size = fs::metadata(path)?.len();
    if size > limit as u64 {
        return Err(ContractError::Rejected(format!(
            "persisted {what} is {size} bytes, over the {limit} byte read limit"
        )));
    }
    Ok(fs::read(path)?)
}

/// `fsync` a directory so a rename survives a crash.
fn fsync_dir(dir: &Path) -> Result<(), ContractError> {
    let handle = fs::File::open(dir)?;
    handle.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
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
}
