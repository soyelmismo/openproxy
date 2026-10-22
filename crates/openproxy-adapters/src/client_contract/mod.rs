//! Common typed base for auto-refreshed client contracts.
//!
//! Every provider contract (Codex, CodeBuddy, OpenCode Zen, OpenCode Go) shares
//! the same envelope: provenance, generation digest, client identity and the
//! compiled rule-set identity, while `payload` stays a provider-typed struct.
//! Nothing in this module executes downloaded code, and no file written here
//! may carry an authorization value, a cookie or a session identifier.
//!
//! Layout of the base (owned by this module, no provider extraction yet):
//!
//! - [`refresh`]: conditional acquisition over a pluggable transport, with a
//!   secret-free transport cache keyed by URL + `Accept` + auth scope.
//! - [`validation`]: structural limits, header hygiene, secret rejection and
//!   stable-channel SemVer anti-downgrade.
//! - [`store`]: atomic multi-file persistence (payload by generation, catalog
//!   by digest, manifest by `fsync` + `rename`) and in-memory `Arc`
//!   publication after validation.
//!
//! Provider drivers (Codex/CodeBuddy/OpenCode) plug into this base later; they
//! own their payload types, parsers and protocol guards.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub mod generic_runtime;
pub mod providers;
pub mod refresh;
pub mod store;
pub mod validation;

pub use generic_runtime::{GenericContractRuntime, ReleaseDriverFactory};

#[cfg(feature = "upstream-hyper")]
pub use refresh::UpstreamHttpTransport;
pub use refresh::{
    CacheEntry, ContractDriver, ContractRefresher, ContractTransport, FetchOutcome, FetchRequest,
    FetchResponse, RefreshReport, SourceDescriptor, TransportCache, TransportCacheKey,
};
pub use store::{ContractStore, MANIFEST_FILE, RefreshReport as StoreRefreshReport, StoreConfig};
pub use validation::{
    ContractLimits, ContractValidator, IdentityTransition, ParsedVersion, VersionChannel,
    candidate_contract, candidate_payload_digest, check_identity_transition,
    contains_secret_material, is_pinned_evidence, is_sha256_hex, parse_version,
    validate_candidate_shape, validate_catalog_body_size, validate_header_pairs,
    validate_no_secrets, validate_payload_size, validate_source_body_size,
};

/// Envelope schema version. Bumped only for a breaking envelope change; the
/// store refuses a manifest carrying any other value.
pub const SCHEMA_VERSION: u32 = 1;

/// Proveedores que poseen un snapshot independiente del contrato.
/// Zen y Go conservan identidades separadas.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContractId {
    ClaudeCode,
    Codex,
    CodeBuddy,
    OpenCodeZen,
    OpenCodeGo,
}

impl ContractId {
    /// All known contracts, in a stable order.
    pub const ALL: [Self; 5] = [
        Self::ClaudeCode,
        Self::Codex,
        Self::CodeBuddy,
        Self::OpenCodeZen,
        Self::OpenCodeGo,
    ];

    /// Stable kebab-case tag. Also the on-disk directory name for a contract.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::CodeBuddy => "codebuddy",
            Self::OpenCodeZen => "opencode-zen",
            Self::OpenCodeGo => "opencode-go",
        }
    }
}

impl std::fmt::Display for ContractId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Identity announced upstream: an optional version plus where it came from.
///
/// `origin` is one of [`IDENTITY_ORIGIN_OFFICIAL`] (the value was read from an
/// official release/package source) or [`IDENTITY_ORIGIN_OWN`] (the provider
/// announces its own identity, e.g. OpenCode Go). `version` stays optional so a
/// provider may declare an identity without a published version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub origin: String,
}

/// Identity read from an official release, package or repository source.
pub const IDENTITY_ORIGIN_OFFICIAL: &str = "official";
/// Identity the provider announces for itself, with no upstream version claim.
pub const IDENTITY_ORIGIN_OWN: &str = "own";

impl ClientIdentity {
    /// Identity with no declared version.
    pub fn unversioned(origin: impl Into<String>) -> Self {
        Self {
            version: None,
            origin: origin.into(),
        }
    }

    /// Identity with a declared version.
    pub fn versioned(version: impl Into<String>, origin: impl Into<String>) -> Self {
        Self {
            version: Some(version.into()),
            origin: origin.into(),
        }
    }
}

/// What kind of evidence a [`SourceRef`] carries. Distinguishes mutable
/// metadata from immutable, revision-pinned material.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EvidenceKind {
    /// Mutable release metadata (`releases/latest`, npm `latest`).
    ReleaseMetadata,
    /// A versioned package manifest (`package.json` at a pinned version).
    PackageManifest,
    /// A content-addressed blob, e.g. a bundle guarded by its own hash.
    BundleHash,
    /// A runtime catalog endpoint (models list), not a versioned contract.
    RuntimeCatalog,
    /// A locally captured fixture, e.g. from an installed CLI.
    CapturedFixture,
}

impl EvidenceKind {
    /// Stable kebab-case tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReleaseMetadata => "release-metadata",
            Self::PackageManifest => "package-manifest",
            Self::BundleHash => "bundle-hash",
            Self::RuntimeCatalog => "runtime-catalog",
            Self::CapturedFixture => "captured-fixture",
        }
    }
}

/// One provenance entry: where a byte range came from and how it is pinned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    pub url: String,
    /// Immutable revision (commit SHA, npm version, git tag) when the source
    /// offers one. `None` only for mutable metadata sources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Lowercase SHA-256 hex digest of the bytes this entry vouches for.
    pub hash: String,
    pub evidence: EvidenceKind,
}

impl SourceRef {
    /// Convenience constructor for a pinned source.
    pub fn pinned(
        url: impl Into<String>,
        revision: impl Into<String>,
        hash: impl Into<String>,
        evidence: EvidenceKind,
    ) -> Self {
        Self {
            url: url.into(),
            revision: Some(revision.into()),
            hash: hash.into(),
            evidence,
        }
    }
}

/// Reference to a separately stored, typed catalog: its digest and format.
/// The catalog body is never inlined into the envelope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogRef {
    /// Lowercase SHA-256 hex digest of the catalog bytes.
    pub digest: String,
    /// Short format tag, e.g. `json`.
    pub format: String,
}

/// A typed candidate produced by a provider driver, not yet activated.
///
/// `payload` is provider-specific: there is no universal Codex-shaped contract.
///
/// `catalog_bytes` is the *runtime* catalog body the candidate carries so the
/// store can persist it digest-addressed before the manifest. It is **not**
/// part of the serialized envelope (`#[serde(skip, default)]`), so a manifest
/// or payload file never inlines a catalog, and it never counts against the
/// profile limit. `catalog_ref` remains the portable, persisted reference:
/// [`CatalogRef::digest`] must equal `hex_digest(catalog_bytes)` whenever both
/// are present.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate<T> {
    pub contract_id: ContractId,
    pub client_identity: ClientIdentity,
    /// Identity of the compiled extraction/guard logic, not of downloaded code.
    pub rule_set_id: String,
    /// Version of the compiled extraction/guard logic.
    pub extractor_version: u32,
    pub sources: Vec<SourceRef>,
    pub payload: T,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_ref: Option<CatalogRef>,
    /// Catalog body held only in memory for this commit. Never serialized, so
    /// it cannot land in the manifest, the payload artifact or any profile
    /// measurement.
    #[serde(skip, default)]
    pub catalog_bytes: Option<Vec<u8>>,
}

impl<T> Candidate<T> {
    /// Candidate with no catalog reference and no catalog bytes.
    pub fn without_catalog(
        contract_id: ContractId,
        client_identity: ClientIdentity,
        rule_set_id: impl Into<String>,
        extractor_version: u32,
        sources: Vec<SourceRef>,
        payload: T,
    ) -> Self {
        Self {
            contract_id,
            client_identity,
            rule_set_id: rule_set_id.into(),
            extractor_version,
            sources,
            payload,
            catalog_ref: None,
            catalog_bytes: None,
        }
    }

    /// Attach a catalog body and derive its reference.
    ///
    /// The digest is computed over the exact bytes passed in, so the persisted
    /// `catalog_ref.digest` is always the digest of the bytes the store will
    /// write to `catalogs/<digest>.<format>`. The format tag is normalized to
    /// lowercase so the artifact name and the reference agree.
    pub fn with_catalog(mut self, bytes: Vec<u8>, format: impl Into<String>) -> Self {
        let digest = hex_digest(&bytes);
        self.catalog_ref = Some(CatalogRef {
            digest,
            format: format.into().to_ascii_lowercase(),
        });
        self.catalog_bytes = Some(bytes);
        self
    }

    /// The catalog body this candidate carries, if any.
    pub fn catalog_body(&self) -> Option<&[u8]> {
        self.catalog_bytes.as_deref()
    }

    /// Whether a declared `catalog_ref` is backed by bytes in this candidate.
    ///
    /// A candidate that declares a reference but carries no bytes is *legal*
    /// (the bytes may already be persisted from an earlier commit), so this is
    /// an inspection helper, not a rejection rule.
    pub fn catalog_backed(&self) -> bool {
        match (&self.catalog_ref, &self.catalog_bytes) {
            (None, None) => true,
            (Some(reference), Some(bytes)) => reference.digest == hex_digest(bytes),
            (_, _) => false,
        }
    }
}

/// The activated envelope: one coherent generation served to every request hook.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot<T> {
    pub schema_version: u32,
    pub contract_id: ContractId,
    /// Digest of the semantic content plus the artifact references. Two
    /// snapshots with the same generation are interchangeable.
    pub generation: String,
    pub client_identity: ClientIdentity,
    pub rule_set_id: String,
    pub extractor_version: u32,
    pub sources: Vec<SourceRef>,
    pub payload: T,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_ref: Option<CatalogRef>,
    /// Runtime catalog body, shared so every request hook reads the same bytes
    /// as the generation it was published with. Never serialized: a manifest
    /// or payload artifact must not inline a catalog, and the profile limit
    /// never counts these bytes.
    #[serde(skip, default)]
    pub catalog: Option<Arc<[u8]>>,
}

impl<T: Clone> Snapshot<T> {
    /// Rebuild the candidate shape of an activated snapshot, e.g. to re-run a
    /// validator over a snapshot loaded from disk. The catalog bytes are
    /// copied into the candidate so a re-validation sees the same body.
    pub fn as_candidate(&self) -> Candidate<T> {
        Candidate {
            contract_id: self.contract_id,
            client_identity: self.client_identity.clone(),
            rule_set_id: self.rule_set_id.clone(),
            extractor_version: self.extractor_version,
            sources: self.sources.clone(),
            payload: self.payload.clone(),
            catalog_ref: self.catalog_ref.clone(),
            catalog_bytes: self.catalog.as_ref().map(|bytes| bytes.to_vec()),
        }
    }

    /// The runtime catalog body, if this snapshot carries one.
    pub fn catalog_body(&self) -> Option<&[u8]> {
        self.catalog.as_deref()
    }
}

impl<T: Serialize> Snapshot<T> {
    /// Build an envelope from a candidate, computing the generation digest.
    ///
    /// Sources are sorted into a canonical order first, so acquisition order
    /// never changes the digest of identical content.
    pub fn from_candidate(candidate: Candidate<T>) -> Result<Self, ContractError> {
        let generation = compute_generation(&candidate)?;
        let mut sources = candidate.sources;
        sort_sources(&mut sources);
        Ok(Self {
            schema_version: SCHEMA_VERSION,
            contract_id: candidate.contract_id,
            generation,
            client_identity: candidate.client_identity,
            rule_set_id: candidate.rule_set_id,
            extractor_version: candidate.extractor_version,
            sources,
            payload: candidate.payload,
            catalog_ref: candidate.catalog_ref,
            catalog: candidate.catalog_bytes.map(Arc::from),
        })
    }

    /// SHA-256 hex digest of the canonical JSON encoding of `payload`. This is
    /// the name of the persisted payload artifact and the value the transport
    /// cache compares to decide `Unchanged`.
    pub fn payload_digest(&self) -> Result<String, ContractError> {
        payload_digest(&self.payload)
    }
}

/// Canonicalize source order so the generation digest is acquisition-order
/// independent.
fn sort_sources(sources: &mut [SourceRef]) {
    sources.sort_by(|a, b| {
        a.url
            .cmp(&b.url)
            .then_with(|| a.revision.cmp(&b.revision))
            .then_with(|| a.hash.cmp(&b.hash))
            .then_with(|| a.evidence.as_str().cmp(b.evidence.as_str()))
    });
}

/// Deterministic canonical JSON bytes for a serializable value.
///
/// `serde_json` without the `preserve_order` feature maps objects to
/// `BTreeMap`, so key order is sorted and the encoding is reproducible.
pub fn canonical_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, ContractError> {
    serde_json::to_vec(value).map_err(ContractError::from)
}

/// Lowercase SHA-256 hex digest of `bytes`.
pub fn hex_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    hex
}

/// SHA-256 hex digest of the canonical JSON encoding of `payload`.
pub fn payload_digest<T: Serialize>(payload: &T) -> Result<String, ContractError> {
    let bytes = canonical_bytes(payload)?;
    Ok(hex_digest(&bytes))
}

/// Canonical projection hashed into the generation digest. Field order is the
/// encoding order; the payload contributes its canonical JSON bytes.
#[derive(Serialize)]
struct GenerationCanonical<'a> {
    schema_version: u32,
    contract_id: &'a ContractId,
    client_identity: &'a ClientIdentity,
    rule_set_id: &'a str,
    extractor_version: u32,
    sources: &'a [SourceRef],
    catalog_ref: Option<&'a CatalogRef>,
    payload_bytes: &'a [u8],
}

/// Digest of the semantic content plus the artifact references.
fn compute_generation<T: Serialize>(candidate: &Candidate<T>) -> Result<String, ContractError> {
    let mut sources = candidate.sources.clone();
    sort_sources(&mut sources);
    let payload_bytes = canonical_bytes(&candidate.payload)?;
    let canonical = GenerationCanonical {
        schema_version: SCHEMA_VERSION,
        contract_id: &candidate.contract_id,
        client_identity: &candidate.client_identity,
        rule_set_id: &candidate.rule_set_id,
        extractor_version: candidate.extractor_version,
        sources: &sources,
        catalog_ref: candidate.catalog_ref.as_ref(),
        payload_bytes: &payload_bytes,
    };
    let bytes = canonical_bytes(&canonical)?;
    Ok(hex_digest(&bytes))
}

/// Errors of the common contract base.
///
/// Every variant is small on purpose: the error is carried by value through
/// `Result` on background refresh paths.
#[derive(Clone, Debug, thiserror::Error)]
pub enum ContractError {
    #[error("transport failure for {url}: {detail}")]
    Transport { url: String, detail: String },
    #[error("source {url} returned unexpected status {status}")]
    UnexpectedStatus { url: String, status: u16 },
    #[error("source {url} is missing")]
    SourceMissing { url: String },
    #[error("source {url} is unavailable with status {status}")]
    SourceUnavailable { url: String, status: u16 },
    #[error("source {url} is rate limited (retry after {retry_after:?})")]
    RateLimited {
        url: String,
        retry_after: Option<u64>,
    },
    #[error("304 without usable cached bytes for {url}")]
    MissingCachedBytes { url: String },
    #[error("cached payload digest {found} does not match the active {expected}")]
    CacheDigestMismatch { expected: String, found: String },
    #[error("candidate carries secret material in {location}")]
    SecretMaterial { location: &'static str },
    #[error("payload is {size} bytes, over the {limit} byte profile limit")]
    PayloadTooLarge { size: usize, limit: usize },
    #[error("source body is {size} bytes, over the {limit} byte limit")]
    SourceTooLarge { size: usize, limit: usize },
    #[error("catalog carries {models} models, over the {limit} model limit")]
    CatalogTooLarge { models: usize, limit: usize },
    #[error("catalog body is {size} bytes, over the {limit} byte catalog limit")]
    CatalogBytesTooLarge { size: usize, limit: usize },
    #[error("invalid header {name:?}: {reason}")]
    InvalidHeader { name: String, reason: &'static str },
    #[error("duplicate header {name:?}")]
    DuplicateHeader { name: String },
    #[error("invalid source url: {reason}")]
    InvalidSourceUrl { reason: &'static str },
    #[error("source hash {value:?} is not a lowercase sha-256 hex digest")]
    InvalidDigest { value: String },
    #[error("unsupported release channel transition from {previous} to {candidate}")]
    ChannelMismatch { previous: String, candidate: String },
    #[error("version downgrade from {previous} to {candidate} rejected")]
    Downgrade { previous: String, candidate: String },
    #[error("candidate drops the declared client version")]
    MissingVersion,
    #[error("version {version:?} is not a three-part semantic version")]
    UnparsableVersion { version: String },
    #[error("version {version:?} changed content: generation {previous} -> {candidate}")]
    VersionConflict {
        version: String,
        previous: String,
        candidate: String,
    },
    #[error("candidate contract {found:?} does not belong to store {expected:?}")]
    ContractIdMismatch {
        found: ContractId,
        expected: ContractId,
    },
    #[error("schema version {found} does not match {expected}")]
    SchemaMismatch { found: u32, expected: u32 },
    #[error("persisted payload digest mismatch: manifest {manifest}, file {file}")]
    PayloadDigestMismatch { manifest: String, file: String },
    #[error("persisted generation {found} does not match the recomputed {expected}")]
    GenerationMismatch { found: String, expected: String },
    #[error("persisted catalog digest mismatch: reference {reference}, file {file}")]
    CatalogDigestMismatch { reference: String, file: String },
    #[error("contract store io failure: {0}")]
    Io(Arc<std::io::Error>),
    #[error("contract encoding failure: {0}")]
    Encoding(Arc<serde_json::Error>),
    #[error("validator rejected the candidate: {0}")]
    Rejected(String),
}

impl From<std::io::Error> for ContractError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(Arc::new(error))
    }
}

impl From<serde_json::Error> for ContractError {
    fn from(error: serde_json::Error) -> Self {
        Self::Encoding(Arc::new(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct DemoPayload {
        user_agent: String,
        headers: Vec<String>,
    }

    fn source(url: &str, revision: &str, hash: &str) -> SourceRef {
        SourceRef::pinned(url, revision, hash, EvidenceKind::PackageManifest)
    }

    fn candidate(version: &str, user_agent: &str) -> Candidate<DemoPayload> {
        Candidate::without_catalog(
            ContractId::Codex,
            ClientIdentity::versioned(version, IDENTITY_ORIGIN_OFFICIAL),
            "codex-rules",
            7,
            vec![
                source("https://b.example/x.json", "rev-b", &"b".repeat(64)),
                source("https://a.example/y.json", "rev-a", &"a".repeat(64)),
            ],
            DemoPayload {
                user_agent: user_agent.to_string(),
                headers: vec!["origin".to_string()],
            },
        )
    }

    #[test]
    fn contract_id_tags_are_stable() {
        assert_eq!(ContractId::Codex.as_str(), "codex");
        assert_eq!(ContractId::OpenCodeGo.as_str(), "opencode-go");
        assert_eq!(ContractId::ClaudeCode.as_str(), "claude-code");
        assert!(ContractId::ALL.contains(&ContractId::ClaudeCode));
        assert_eq!(ContractId::ALL.len(), 5);
    }

    #[test]
    fn hex_digest_is_lowercase_sha256() {
        let digest = hex_digest(b"");
        assert_eq!(digest.len(), 64);
        assert_eq!(
            digest,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(is_sha256_hex(&digest));
    }

    #[test]
    fn generation_is_independent_of_source_order() {
        let mut reordered = candidate("1.2.3", "codex-cli/1.2.3");
        reordered.sources.reverse();
        let first = Snapshot::from_candidate(candidate("1.2.3", "codex-cli/1.2.3")).unwrap();
        let second = Snapshot::from_candidate(reordered).unwrap();
        assert_eq!(first.generation, second.generation);
        assert_eq!(first.sources[0].url, "https://a.example/y.json");
    }

    #[test]
    fn generation_changes_with_payload_and_with_metadata() {
        let base = Snapshot::from_candidate(candidate("1.2.3", "codex-cli/1.2.3")).unwrap();
        let other_payload =
            Snapshot::from_candidate(candidate("1.2.3", "codex-cli/9.9.9")).unwrap();
        let other_version =
            Snapshot::from_candidate(candidate("1.2.4", "codex-cli/1.2.3")).unwrap();
        assert_ne!(base.generation, other_payload.generation);
        assert_ne!(base.generation, other_version.generation);
    }

    #[test]
    fn snapshot_roundtrip_keeps_envelope() {
        let snapshot = Snapshot::from_candidate(candidate("2.0.1", "codex-cli/2.0.1")).unwrap();
        let bytes = serde_json::to_vec(&snapshot).unwrap();
        let loaded: Snapshot<DemoPayload> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(loaded.schema_version, SCHEMA_VERSION);
        assert_eq!(loaded.generation, snapshot.generation);
        assert_eq!(loaded.payload, snapshot.payload);
        assert_eq!(loaded.as_candidate().sources.len(), 2);
        assert_eq!(
            loaded.payload_digest().unwrap(),
            snapshot.payload_digest().unwrap()
        );
    }

    #[test]
    fn optional_catalog_ref_survives_roundtrip() {
        let mut typed = candidate("2.0.1", "codex-cli/2.0.1");
        typed.catalog_ref = Some(CatalogRef {
            digest: "c".repeat(64),
            format: "json".to_string(),
        });
        let snapshot = Snapshot::from_candidate(typed).unwrap();
        let bytes = serde_json::to_vec(&snapshot).unwrap();
        let loaded: Snapshot<DemoPayload> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(loaded.catalog_ref, snapshot.catalog_ref);

        let plain = Snapshot::from_candidate(candidate("2.0.1", "codex-cli/2.0.1")).unwrap();
        let encoded = serde_json::to_string(&plain).unwrap();
        assert!(!encoded.contains("catalog_ref"));
    }

    #[test]
    fn catalog_bytes_stay_out_of_the_serialized_envelope() {
        let bytes = br#"{"models":["gpt-5"]}"#.to_vec();
        let typed = candidate("2.0.1", "codex-cli/2.0.1").with_catalog(bytes.clone(), "JSON");
        assert_eq!(
            typed.catalog_ref.as_ref().expect("reference").format,
            "json"
        );
        assert_eq!(typed.catalog_body(), Some(bytes.as_slice()));
        assert!(typed.catalog_backed());

        let snapshot = Snapshot::from_candidate(typed).unwrap();
        assert_eq!(snapshot.catalog_body(), Some(bytes.as_slice()));
        let encoded = serde_json::to_string(&snapshot).unwrap();
        assert!(
            !encoded.contains("models"),
            "catalog bytes must not serialize"
        );
        assert!(!encoded.contains("catalog_bytes"));

        let loaded: Snapshot<DemoPayload> = serde_json::from_str(&encoded).unwrap();
        assert_eq!(loaded.catalog_ref, snapshot.catalog_ref);
        assert_eq!(loaded.generation, snapshot.generation);
        assert!(
            loaded.catalog.is_none(),
            "catalog bytes are runtime-only and must not round-trip through the envelope"
        );

        let plain = Snapshot::from_candidate(candidate("2.0.1", "codex-cli/2.0.1")).unwrap();
        assert_ne!(
            plain.generation, snapshot.generation,
            "a declared catalog reference must change the generation"
        );
    }
}
