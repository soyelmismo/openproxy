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
            let bytes = self.read_verified_catalog(reference)?;
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
                let reread = self.read_verified_catalog(reference)?;
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
                let bytes = self.read_verified_catalog(reference)?;
                Ok(Some(Arc::from(bytes)))
            }
        }
    }

    /// Read a catalog artifact under the catalog limit and verify it against
    /// the reference digest, so a torn or tampered catalog is never activated.
    fn read_verified_catalog(&self, reference: &CatalogRef) -> Result<Vec<u8>, ContractError> {
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
        Ok(bytes)
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
mod tests;
