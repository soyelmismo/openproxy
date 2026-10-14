# `client_contract` — API contract (implemented)

Typed, provider-agnostic base for auto-refreshing contract sources
(Codex, CodeBuddy, OpenCode Zen, OpenCode Go). Everything below is
already implemented in this module tree; nothing is aspirational.

Ownership: this doc and the four source files live in
`crates/openproxy-adapters/src/client_contract/`. `providers/` holds the
per-provider drivers and payload types; `fixtures/` holds captured upstream
bodies. Both are maintained outside this module's scope.

```
client_contract/
├── mod.rs          envelope, identity, provenance, digest, errors, exports
├── refresh.rs      conditional fetch, pluggable transport, secret-free cache
├── validation.rs   limits, structural checks, header hygiene, SemVer, anti-downgrade
├── store.rs        atomic catalog + payload + manifest persistence, Arc publication
└── api.md          this file
```

No new dependency was added. Existing deps only:
`serde`, `serde_json`, `sha2`, `chrono`, `parking_lot`, `bytes`, `http`,
`tokio`, `thiserror`, `rand`.

---

## 1. Identity

```rust
pub enum ContractId { Codex, CodeBuddy, OpenCodeZen, OpenCodeGo }

impl ContractId {
    pub const fn as_str(self) -> &'static str;
}
```
`as_str` is the canonical lowercase file-name component (`codex`,
`codebuddy`, `opencode-zen`, `opencode-go`). Unknown ids are mapped in by the
parent; the base accepts any known `ContractId` and rejects only a mismatch
between a candidate and its store.

---

## 2. Envelope

```rust
pub const SCHEMA_VERSION: u32 = 1;

pub struct Snapshot<T> {
    pub schema_version: u32,
    pub contract_id: ContractId,
    pub generation: String,                      // sha256 hex
    pub client_identity: ClientIdentity,
    pub rule_set_id: String,                     // compiled rule-set id
    pub extractor_version: u32,
    pub sources: Vec<SourceRef>,
    pub payload: T,                              // provider-specific, typed
    pub catalog_ref: Option<CatalogRef>,
    /// Runtime catalog body. Never serialized; never counted in the profile
    /// limit. Shared as `Arc<[u8]>` so every request hook reads the exact
    /// bytes its generation was published with.
    #[serde(skip, default)]
    pub catalog: Option<Arc<[u8]>>,
}

impl<T: Clone> Snapshot<T> {
    pub fn as_candidate(&self) -> Candidate<T>;
    pub fn catalog_body(&self) -> Option<&[u8]>;
}

impl<T: serde::Serialize> Snapshot<T> {
    pub fn from_candidate(candidate: Candidate<T>) -> Result<Self, ContractError>;
    pub fn payload_digest(&self) -> Result<String, ContractError>;
}

pub struct ClientIdentity {
    pub version: Option<String>,
    pub origin: String,
}
impl ClientIdentity {
    pub fn versioned(version: impl Into<String>, origin: impl Into<String>) -> Self;
    pub fn unversioned(origin: impl Into<String>) -> Self;
}

pub const IDENTITY_ORIGIN_OFFICIAL: &str = "official";
pub const IDENTITY_ORIGIN_OWN: &str = "own";

pub struct SourceRef {
    pub url: String,
    /// Immutable revision (commit SHA, npm version, git tag) when the source
    /// offers one. `None` only for mutable metadata sources.
    pub revision: Option<String>,
    pub hash: String,          // sha256 hex, 64 chars
    pub evidence: EvidenceKind,
}
impl SourceRef {
    pub fn pinned(
        url: impl Into<String>, revision: impl Into<String>,
        hash: impl Into<String>, evidence: EvidenceKind,
    ) -> Self;
}

pub enum EvidenceKind {
    ReleaseMetadata,   // mutable release metadata (`releases/latest`, npm `latest`)
    PackageManifest,   // versioned package manifest at a pinned version
    BundleHash,        // content-addressed blob guarded by its own hash
    RuntimeCatalog,    // runtime models-list endpoint, not a versioned contract
    CapturedFixture,   // locally captured fixture, e.g. from an installed CLI
}

pub struct CatalogRef { pub digest: String, pub format: String }
```

`generation` is `sha256(schema_version ‖ contract_id ‖ canonical_json(payload)
‖ canonical identity ‖ rule_set_id ‖ extractor_version ‖ sorted sources)`.
It is deterministic: identical content never produces two generations, and
any byte change produces a different one.

---

## 3. Candidate (typed, never a universal Codex shape)

```rust
pub struct Candidate<T> {
    pub contract_id: ContractId,
    pub client_identity: ClientIdentity,
    pub rule_set_id: String,
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
    pub fn without_catalog(
        contract_id: ContractId, client_identity: ClientIdentity,
        rule_set_id: impl Into<String>, extractor_version: u32,
        sources: Vec<SourceRef>, payload: T,
    ) -> Self;

    /// Attach a catalog body and derive its reference. The digest is computed
    /// over the exact bytes passed in and the format tag is normalized to
    /// lowercase, so the artifact name and the reference always agree.
    pub fn with_catalog(mut self, bytes: Vec<u8>, format: impl Into<String>) -> Self;

    pub fn catalog_body(&self) -> Option<&[u8]>;
    /// Whether a declared `catalog_ref` is backed by bytes in this candidate.
    /// A reference without bytes is legal (the bytes may already be
    /// persisted), so this is an inspection helper, not a rejection rule.
    pub fn catalog_backed(&self) -> bool;
}
```
`T` is the provider payload type. There is no `Codex`-shaped untyped
variant anywhere in the base.

---

## 4. Errors

```rust
#[derive(Clone, Debug, thiserror::Error)]
pub enum ContractError {
    Transport { url: String, detail: String },
    UnexpectedStatus { url: String, status: u16 },
    SourceMissing { url: String },
    SourceUnavailable { url: String, status: u16 },
    RateLimited { url: String, retry_after: Option<u64> },
    MissingCachedBytes { url: String },
    CacheDigestMismatch { expected: String, found: String },
    SecretMaterial { location: &'static str },
    PayloadTooLarge { size: usize, limit: usize },
    SourceTooLarge { size: usize, limit: usize },
    CatalogTooLarge { models: usize, limit: usize },
    CatalogBytesTooLarge { size: usize, limit: usize },
    InvalidHeader { name: String, reason: &'static str },
    DuplicateHeader { name: String },
    InvalidSourceUrl { reason: &'static str },
    InvalidDigest { value: String },
    ChannelMismatch { previous: String, candidate: String },
    Downgrade { previous: String, candidate: String },
    MissingVersion,
    UnparsableVersion { version: String },
    VersionConflict { version: String, previous: String, candidate: String },
    ContractIdMismatch { found: ContractId, expected: ContractId },
    SchemaMismatch { found: u32, expected: u32 },
    PayloadDigestMismatch { manifest: String, file: String },
    GenerationMismatch { found: String, expected: String },
    CatalogDigestMismatch { reference: String, file: String },
    Io(Arc<std::io::Error>),
    Encoding(Arc<serde_json::Error>),
    Rejected(String),
}
```

`Io` and `Encoding` hold an `Arc` so the enum stays `Clone`, which background
refresh paths need (`RefreshReport::Rejected { reason: ContractError }`). The
`From` impls wrap the raw errors.

---

## 5. Validation

```rust
pub struct ContractLimits {
    pub profile_bytes: usize,    // 64 KiB payload cap (default)
    pub catalog_bytes: usize,    // 4 MiB catalog body (default)
    pub catalog_models: usize,   // 1000 entries (default)
    pub source_bytes: usize,     // 8 MiB per fetched source body (default)
    pub max_sources: usize,      // 32 provenance entries (default)
    pub max_header_bytes: usize, // 8 KiB per header name or value (default)
    pub max_redirects: usize,    // 5 accepted HTTP jumps (default)
}
impl ContractLimits { pub fn default() -> Self; pub fn tight() -> Self; }

pub trait ContractValidator<T>: Send + Sync {
    fn rule_set_id(&self) -> &'static str;
    fn extractor_version(&self) -> u32;
    fn validate(
        &self,
        candidate: &Candidate<T>,
        previous: Option<&Snapshot<T>>,
        limits: &ContractLimits,
    ) -> Result<(), ContractError>;

    /// Optional provider tightening. The base still enforces its own cap.
    fn catalog_capacity(&self, candidate: &Candidate<T>) -> Option<usize> { None }
}

pub fn validate_candidate_shape<T: serde::Serialize>(
    candidate: &Candidate<T>, limits: &ContractLimits,
) -> Result<(), ContractError>;

pub fn validate_payload_size<T: serde::Serialize>(
    candidate: &Candidate<T>, limits: &ContractLimits,
) -> Result<usize, ContractError>;          // returns measured size

pub fn validate_header_pairs(
    headers: &[(String, String)],
) -> Result<(), ContractError>;

pub fn contains_secret_material(bytes: &[u8]) -> bool;
pub fn is_sha256_hex(value: &str) -> bool;
```

Rules enforced by the base (a provider may tighten, never loosen):

- payload JSON ≤ `profile_bytes` (default 64 KiB), measured after
  decompression;
- every `SourceRef.hash` is 64-char lowercase hex;
- `SourceRef.url` parses as an absolute `https`/`http` URL with a host;
- `catalog_ref.digest` is sha256 hex and `format` is non-empty;
- when `catalog_bytes` are present they are bounded by `catalog_bytes`, free
  of credential material, and their digest must equal `catalog_ref.digest`;
- `schema_version == SCHEMA_VERSION`;
- header names are valid tokens, values contain no CR/LF, and no pair may use
  an auth header name (`authorization`, `proxy-authorization`,
  `www-authenticate`, `cookie`, `set-cookie`, `x-api-key`, `api-key`,
  `x-goog-api-key`, `x-auth-token`) — credential values belong to the runtime,
  not to a contract;
- no credential material anywhere in the serialized candidate, the source URLs,
  the source revisions or the catalog body (see the scanner below);
- `client_identity.origin` is `official` or `own`.

The secret scanner is value-sensitive, not name-sensitive. A credential *name*
on its own is documentation and is allowed: a guard symbol such as
`insert_header(&mut headers, "session-id", &id);` or a payload key
`"session-id"` carries no credential. What is rejected is a name bound to a
value:

- a credential name followed by `:` or `=` (`session_id=abc`, `token: …`,
  `Authorization: Bearer sk-…`);
- a JSON credential key carrying a non-empty, non-null value
  (`"token":"abc"`), while `"token":""` and `"token":null` are placeholders;
- an auth scheme (`bearer `, `basic `) followed by a base64-shaped value —
  12+ characters carrying `+`/`/`/`=` padding, two or more upper-case letters,
  or a digit beside an upper-case letter — so `Basic dXNlcjpwYXNz` is rejected
  while `basic searches` and `Bearer authentication` are not.

Header names are never hidden, renamed or hashed to pass this check. The last
rule is a documented heuristic: it exists so the word `basic` inside a model
description is not a false positive, and every real credential value is caught
by the name-binding rules before it.

Anti-downgrade and SemVer live in `check_identity_transition`:

```rust
pub enum VersionChannel { Stable, Prerelease }
pub struct ParsedVersion { pub channel: VersionChannel, pub core: [u64; 3], pub normalized: String }
pub fn parse_version(raw: &str) -> Result<ParsedVersion, ContractError>;
pub enum IdentityTransition { New, Same, Newer }
pub fn check_identity_transition(
    previous: &ClientIdentity, next: &ClientIdentity,
) -> Result<IdentityTransition, ContractError>;
```
- `New` = no previous, or first time an unversioned id becomes versioned.
- `Newer` = higher SemVer, or a move into a stricter channel.
- `Same` = identical version, so the store compares generations and rejects a
  content change as `VersionConflict`.
- A lower version returns `Downgrade`; a move into a looser channel returns
  `ChannelMismatch`.

---

## 6. Refresh (conditional, secret-free cache)

```rust
pub struct SourceDescriptor {
    pub url: String,
    pub accept: String,
    pub auth_scope: String,     // opaque label, never a credential
    pub headers: Vec<(String, String)>,
    pub evidence: EvidenceKind,
    pub revision: Option<String>,
}

pub enum FetchOutcome<T> { Unchanged, Candidate(Candidate<T>) }

pub enum RefreshReport<T> {
    Unchanged { generation: String },
    Rejected { reason: ContractError },
    Activated { generation: String, snapshot: Arc<Snapshot<T>> },
}
// `RefreshReport` is defined once in refresh.rs; `store.rs` re-exports it as
// `StoreRefreshReport`, and `mod.rs` publishes both names.

pub trait ContractTransport: Send + Sync {
    fn fetch(&self, request: &FetchRequest)
        -> Pin<Box<dyn Future<Output = Result<FetchResponse, ContractError>> + Send + '_>>;
}

pub trait ContractDriver<T>: Send + Sync {
    fn contract_id(&self) -> ContractId;
    fn rule_set_id(&self) -> &'static str;
    fn extractor_version(&self) -> u32;
    fn source(&self) -> SourceDescriptor;
    fn parse(&self, body: &[u8], revision: Option<&str>, body_digest: &str)
        -> Result<Candidate<T>, ContractError>;
}

pub struct ContractRefresher<T> { /* private */ }

impl<T> ContractRefresher<T>
where
    T: serde::Serialize + Send + Sync + 'static,
{
    pub fn new(
        transport: Arc<dyn ContractTransport>,
        cache: Arc<TransportCache>,
        limits: ContractLimits,
        driver: Arc<dyn ContractDriver<T>>,
    ) -> Self;

    pub fn contract_id(&self) -> ContractId;
    pub fn cache(&self) -> &Arc<TransportCache>;
    pub async fn refresh(&self) -> Result<FetchOutcome<T>, ContractError>;
}

pub struct FetchRequest {
    pub key: TransportCacheKey,
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
}

pub struct FetchResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}
impl FetchResponse {
    pub fn etag(&self) -> Option<String>;
    pub fn last_modified(&self) -> Option<String>;
    pub fn retry_after_secs(&self) -> Option<u64>;
}

pub struct TransportCacheKey { url, accept, auth_scope }   // private fields
impl TransportCacheKey {
    pub fn new(url: impl Into<String>, accept: impl Into<String>,
               auth_scope: impl Into<String>) -> Self;
    pub fn digest(&self) -> String;   // sha256 hex of url ‖ accept ‖ auth_scope
}

pub struct TransportCache;            // Arc<Mutex<HashMap<..>>>, secret-free
impl TransportCache {
    pub fn new() -> Self;
    pub fn get(&self, key: &TransportCacheKey) -> Option<CacheEntry>;
    pub fn insert(&self, entry: CacheEntry);
    pub fn touch(&self, key: &TransportCacheKey, at: chrono::DateTime<Utc>);
    pub fn invalidate(&self, key: &TransportCacheKey);
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}
```

### 304 policy (exact)

1. Send the conditional request with `If-None-Match` (and
   `If-Modified-Since` when available) only when a replayable entry exists.
2. On `304`:
   - if there is **no** cached entry, or the entry's stored digest does not
     match a re-computed digest of the held bytes, do **one** unconditional
     retry (no conditional headers) and treat its body as the candidate;
   - if that retry also answers `304`, return `MissingCachedBytes`.
   - otherwise return `FetchOutcome::Unchanged`.
3. On `200` with a body whose digest equals the cached digest, return
   `Unchanged` even if the ETag changed, and refresh the stored validators.
4. The cache stores only `url ‖ accept ‖ auth_scope`, the ETag, the
   last-modified timestamp, a digest, the byte count and the body bytes.
   It never stores an `Authorization`-style value.

### Production transport (feature-gated, not a stub)

```rust
#[cfg(feature = "upstream-hyper")]
pub struct UpstreamHttpTransport {
    client: Arc<crate::upstream::UpstreamClient>,
}
#[cfg(feature = "upstream-hyper")]
impl UpstreamHttpTransport {
    pub fn new(client: std::sync::Arc<crate::upstream::UpstreamClient>) -> Self;
    pub fn shared() -> Self;
}
#[cfg(feature = "upstream-hyper")]
impl ContractTransport for UpstreamHttpTransport { /* fetch() */ }
```
It reuses the crate's pooled `UpstreamClient` with
`TimeoutProfile::ModelDiscovery` and a fresh `CancellationToken`, so a
contract refresh inherits the same pool/TLS/SSRF posture as every other
upstream call. The parent wires it or substitutes its own.

---

## 7. Store (atomic persistence, `Arc` publication)

```rust
pub const MANIFEST_FILE: &str = "manifest.json";

pub struct StoreConfig {
    pub root: PathBuf,               // created on first commit
    pub limits: ContractLimits,
}
impl StoreConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self;
    pub fn with_limits(mut self, limits: ContractLimits) -> Self;
}

pub struct ContractStore<T> { /* private */ }

impl<T> ContractStore<T>
where
    T: serde::Serialize + serde::de::DeserializeOwned + Clone + Send + Sync + 'static,
{
    pub fn new(
        contract: ContractId,
        config: StoreConfig,
        embedded: Option<Snapshot<T>>,          // compiled-in seed
        validator: Arc<dyn ContractValidator<T>>,
    ) -> Self;

    /// Read + validate the persisted snapshot and publish it in memory.
    /// Falls back to the embedded seed — with a warning and a full
    /// re-validation — when the manifest is absent or its state is refused.
    pub fn load(&self) -> Result<Arc<Snapshot<T>>, ContractError>;

    /// Validate, anti-downgrade-check and conflict-check, then persist and
    /// publish. Returns Some only when a new snapshot replaces the old one.
    pub fn commit(
        &self,
        candidate: &Candidate<T>,
    ) -> Result<Option<Arc<Snapshot<T>>>, ContractError>;

    pub fn snapshot(&self) -> Option<Arc<Snapshot<T>>>;
    pub fn root(&self) -> &Path;
    pub fn contract_id(&self) -> ContractId;
}

`Some` means "published", `None` means "content was byte-identical, nothing
written".

File layout under the store root:

```
<root>/
├── <contract>.manifest.json           <- published atomically (rename)
├── <contract>-<generation>.json       <- payload, addressed by generation
└── catalogs/<digest>.<format>         <- catalog body, addressed by digest
```

`MANIFEST_FILE` is `"manifest.json"`; the manifest's real name is
`<contract>.manifest.json`, so several contracts can share one root.

`commit` order (nothing is visible before it is durable):

1. `candidate.contract_id` must equal the store's, else `ContractIdMismatch`.
2. Run the provider validator (may tighten), then the base shape check
   (`validate_candidate_shape`).
3. `check_identity_transition` against the previous snapshot: downgrade /
   channel change / same-version-different-content all fail **before** any
   write.
4. Build the candidate snapshot (`from_candidate`) and verify the generation
   is a safe artifact name. A generation identical to the active one returns
   `Ok(None)` — nothing is rewritten.
5. Write the catalog artifact to `catalogs/<digest>.<format>` when the
   candidate carries bytes, re-read it under the catalog limit and re-hash it;
   a candidate that only declares a `catalog_ref` must find that artifact
   already persisted and matching.
6. Write `<contract>-<generation>.json` into a temp file, `sync_all`, `rename`.
7. Serialize the manifest to `<contract>.manifest.json` via a temp file,
   `sync_all`, `rename`, then fsync the parent directory.
8. Only then store the new `Arc<Snapshot<T>>` in the `RwLock`.

The whole sequence runs under one `RwLock::write()` on the store's current
snapshot, so two concurrent commits cannot both validate against the same
previous snapshot and then publish out of order. Catalog bytes are never
serialized into the manifest or the payload artifact and never count against
`profile_bytes`.

`load` refuses a manifest whose `schema_version` differs, whose contract
differs, whose payload file is missing, whose payload digest does not match
the manifest's, or whose recomputed generation differs from the stored one; it
also re-reads and re-hashes the catalog artifact before attaching it. When the
persisted state is refused and an embedded seed exists, the seed is served with
a `tracing::warn!` and re-validated through the same shape check and provider
validator; without a seed the original error stands.

---

## 8. Tests (offline)

| File | Tests | Covers |
|---|---|---|
| `src/client_contract/mod.rs` | 7 | envelope, digest determinism, provenance, limits |
| `src/client_contract/validation.rs` | 18 | header CRLF/dup/length, secret rejection (names vs values), digest/hygiene, SemVer channels, transitions |
| `src/client_contract/refresh.rs` | 6 | 304 policy, refetch-on-304, cache key separation, cache replay, JSON detection, oversized source, HTML rejection, status mapping |
| `src/client_contract/store.rs` | 14 | restart, seed, seed-fallback on corrupt state, no-snapshot, identical, conflict, downgrade, anti-downgrade, rejection, oversize, io failure, tamper, orphan |
| `tests/client_contract_refresh.rs` | 6 | same as above through the public API |
| `tests/client_contract_store.rs` | 18 | restart, io error, tamper, orphan, serde round-trip |

The provider suites under `providers/` and `tests/providers_*.rs` are counted
separately and are outside this module's ownership.

Every test uses a scripted `ContractTransport` and a `temp_dir()` under
`std::env::temp_dir()`; no test touches the network and none holds a
`parking_lot` guard across an `await` (`await_holding_lock` is `deny` in this
workspace).

---

## 9. Follow-up for the parent

- `crates/openproxy-adapters/src/lib.rs` exports `pub mod client_contract;`,
  so everything above is reachable and the tests in `tests/` compile.
- `client_contract/providers/` holds the per-provider payload types,
  `ContractDriver` impls and protocol guards. The base owns the envelope,
  validation, refresh and store; providers only produce `Candidate<T>`.
- `client_contract/fixtures/` holds captured upstream bodies used by the
  provider tests, so no provider test needs the network either.
