//! Conditional acquisition and a secret-free transport cache.
//!
//! One refresh operation per contract returns either [`FetchOutcome::Unchanged`]
//! or a full candidate. There is no separate revalidate GET followed by a fetch
//! GET: the conditional request is the fetch, and a `304` is only accepted when
//! the previous bytes are still on hand and still match the recorded digest.
//!
//! The cache is keyed by the full URL, the negotiated representation (`Accept`)
//! and an auth scope *label*. No credential value, cookie or session identifier
//! is ever a cache key or a cache field: an ETag is opaque and is never treated
//! as a version identity or as cryptographic proof.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::validation::{ContractLimits, validate_source_body_size};
use super::{Candidate, ContractError, ContractId, EvidenceKind, hex_digest, payload_digest};

/// Cache key of one resource: the full URL, the negotiated representation and
/// the auth scope label. The label is a scope name (`public`, `npm-public`,
/// `github-public`), never a credential.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TransportCacheKey {
    pub url: String,
    pub accept: String,
    pub auth_scope: String,
}

impl TransportCacheKey {
    /// Build a key from its three parts.
    pub fn new(
        url: impl Into<String>,
        accept: impl Into<String>,
        auth_scope: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            accept: accept.into(),
            auth_scope: auth_scope.into(),
        }
    }

    /// Stable digest of the key, used as the in-memory map key and as the
    /// on-disk cache file name. Contains no secret by construction: its inputs
    /// are the URL, the Accept value and a scope label.
    ///
    /// The three parts are hashed verbatim. A URL path and query are
    /// case-sensitive (`/Models.json` and `/models.json` are different
    /// resources) and an `Accept` value or a scope label may be too, so folding
    /// case here would let two distinct resources share one cache entry and
    /// serve the wrong bytes on a `304`.
    pub fn digest(&self) -> String {
        let encoded = format!("{}\n{}\n{}", self.url, self.accept, self.auth_scope);
        hex_digest(encoded.as_bytes())
    }
}

/// One cached resource: the previous bytes, their digest, and the validators a
/// conditional request may replay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEntry {
    pub key: TransportCacheKey,
    /// Opaque ETag as received. Never parsed as a version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
    pub checked_at: DateTime<Utc>,
    /// SHA-256 hex of the bytes held in `body`.
    pub digest: String,
    /// Number of bytes held in `body`.
    pub bytes: usize,
    /// The previous body, kept only so a `304` can be served from cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Vec<u8>>,
}

impl CacheEntry {
    /// Whether this entry can answer a `304`, i.e. it holds its previous bytes.
    pub fn is_replayable(&self) -> bool {
        self.body.is_some()
    }

    /// Whether the held body still matches the recorded digest.
    pub fn body_matches_digest(&self) -> bool {
        match &self.body {
            Some(body) => hex_digest(body) == self.digest && body.len() == self.bytes,
            None => false,
        }
    }

    /// The conditional headers to replay, if this entry carries a validator.
    pub fn conditional_headers(&self) -> Vec<(String, String)> {
        let mut headers = Vec::with_capacity(2);
        if let Some(etag) = &self.etag {
            headers.push(("If-None-Match".to_string(), etag.clone()));
        }
        if let Some(last_modified) = &self.last_modified {
            headers.push(("If-Modified-Since".to_string(), last_modified.clone()));
        }
        headers
    }
}

/// In-memory transport cache, shared across refreshes of every contract.
///
/// The map is behind one `Mutex` because every operation is a small clone or a
/// single insert; contention is bounded by the number of contracts, not by
/// request volume, since nothing on an inference path touches this.
#[derive(Debug, Default)]
pub struct TransportCache {
    entries: Mutex<HashMap<String, CacheEntry>>,
}

impl TransportCache {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up the entry for `key`.
    pub fn get(&self, key: &TransportCacheKey) -> Option<CacheEntry> {
        self.entries.lock().get(&key.digest()).cloned()
    }

    /// Insert or replace the entry for `key`.
    pub fn insert(&self, entry: CacheEntry) {
        self.entries.lock().insert(entry.key.digest(), entry);
    }

    /// Record that `key` was checked at `at`, keeping its validators and bytes.
    pub fn touch(&self, key: &TransportCacheKey, at: DateTime<Utc>) {
        if let Some(entry) = self.entries.lock().get_mut(&key.digest()) {
            entry.checked_at = at;
        }
    }

    /// Drop the entry for `key`, e.g. after a digest mismatch.
    pub fn invalidate(&self, key: &TransportCacheKey) {
        self.entries.lock().remove(&key.digest());
    }

    /// Number of live entries.
    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    /// Whether the cache holds no entry.
    pub fn is_empty(&self) -> bool {
        self.entries.lock().is_empty()
    }
}

/// A request the refresher needs to perform, fully specified.
#[derive(Clone, Debug)]
pub struct FetchRequest {
    pub key: TransportCacheKey,
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
}

/// A response as the refresher sees it, transport-agnostic.
#[derive(Clone, Debug)]
pub struct FetchResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl FetchResponse {
    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The opaque ETag, if the server sent one.
    pub fn etag(&self) -> Option<String> {
        self.header("etag").map(str::trim).map(str::to_string)
    }

    /// `Last-Modified`, if the server sent one.
    pub fn last_modified(&self) -> Option<String> {
        self.header("last-modified")
            .map(str::trim)
            .map(str::to_string)
    }

    /// `Retry-After` in seconds, when it is a plain delay.
    pub fn retry_after_secs(&self) -> Option<u64> {
        self.header("retry-after")
            .and_then(|value| value.trim().parse::<u64>().ok())
    }
}

/// The transport a refresher performs its requests over.
///
/// This is the seam the parent wires to the real HTTP client. The production
/// implementation is [`UpstreamHttpTransport`], built on the crate's existing
/// [`crate::upstream::UpstreamClient`]; tests substitute a scripted transport,
/// so no test needs a network.
pub trait ContractTransport: Send + Sync {
    /// Perform one request. Implementations own retries at the connection
    /// level; the refresher interprets the status.
    fn fetch(
        &self,
        request: &FetchRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<FetchResponse, ContractError>> + Send + '_>,
    >;
}

/// The production [`ContractTransport`]: the crate's own [`UpstreamClient`].
///
/// Nothing here invents an HTTP stack — it reuses the pooled, timeout-governed
/// client the rest of the crate already talks through, so a contract refresh
/// inherits the same connection pool, TLS setup and SSRF posture as every other
/// upstream call.
#[cfg(feature = "upstream-hyper")]
pub struct UpstreamHttpTransport {
    client: std::sync::Arc<crate::upstream::UpstreamClient>,
}

#[cfg(feature = "upstream-hyper")]
impl UpstreamHttpTransport {
    /// Wrap an existing client, so a caller shares one pool with everything
    /// else it does.
    pub fn new(client: std::sync::Arc<crate::upstream::UpstreamClient>) -> Self {
        Self { client }
    }

    /// Build a transport around a fresh client.
    pub fn shared() -> Self {
        Self {
            client: crate::upstream::UpstreamClient::new(),
        }
    }
}

#[cfg(feature = "upstream-hyper")]
impl ContractTransport for UpstreamHttpTransport {
    fn fetch(
        &self,
        request: &FetchRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<FetchResponse, ContractError>> + Send + '_>,
    > {
        // The returned future borrows only `self`: everything it needs from
        // the request is moved in, so it is not tied to the caller's `request`
        // lifetime and satisfies the `'_` return bound.
        let url = request.url.clone();
        let headers = request.headers.clone();
        Box::pin(async move {
            let mut spec = crate::upstream::UpstreamRequest::get(url.clone());
            // Non-streaming: a contract source is a small document, so the
            // body-gap timeout would only add a false failure mode.
            spec.is_streaming = false;
            for (name, value) in &headers {
                let Ok(name) = http::header::HeaderName::from_bytes(name.as_bytes()) else {
                    return Err(ContractError::InvalidHeader {
                        name: name.clone(),
                        reason: "header name is not a valid token",
                    });
                };
                let Ok(value) = http::header::HeaderValue::from_str(value) else {
                    return Err(ContractError::InvalidHeader {
                        name: name.to_string(),
                        reason: "header value contains control characters",
                    });
                };
                spec.headers.insert(name, value);
            }
            let cancel = crate::upstream::CancellationToken::new();
            let response = self
                .client
                .call(
                    spec,
                    crate::upstream::TimeoutProfile::ModelDiscovery,
                    cancel,
                )
                .await
                .map_err(|error| ContractError::Transport {
                    url: url.clone(),
                    detail: error.to_string(),
                })?;
            let status = response.status.as_u16();
            let headers: Vec<(String, String)> = response
                .headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_string(),
                        value.to_str().unwrap_or_default().to_string(),
                    )
                })
                .collect();
            let body = response
                .collect()
                .await
                .map_err(|error| ContractError::Transport {
                    url: url.clone(),
                    detail: error.to_string(),
                })?;
            Ok(FetchResponse {
                status,
                headers,
                body: body.to_vec(),
            })
        })
    }
}

/// Where a driver should acquire a resource from.
#[derive(Clone, Debug)]
pub struct SourceDescriptor {
    pub url: String,
    /// Negotiated representation, e.g. `application/json`.
    pub accept: String,
    /// Auth scope label for the cache key. Never a credential.
    pub auth_scope: String,
    /// Extra headers to send. Credentials are added by the transport, not here.
    pub headers: Vec<(String, String)>,
    /// Evidence kind the bytes represent.
    pub evidence: EvidenceKind,
    /// Immutable revision, when the source is pinned.
    pub revision: Option<String>,
}

/// Provider-specific acquisition and parsing for one contract.
///
/// One driver owns one payload type. There is no universal Codex-shaped
/// candidate: CodeBuddy reads `product.json`, OpenCode reads the CLI package,
/// Codex reads its release and repository sources.
pub trait ContractDriver<T>: Send + Sync {
    /// The contract this driver refreshes.
    fn contract_id(&self) -> ContractId;

    /// Identity of the compiled extraction and guard logic.
    fn rule_set_id(&self) -> &'static str;

    /// Version of the compiled extraction and guard logic.
    fn extractor_version(&self) -> u32;

    /// The single resource to acquire this round.
    fn source(&self) -> SourceDescriptor;

    /// Parse the acquired bytes into a typed candidate.
    ///
    /// `revision` is the immutable revision the source descriptor declared, if
    /// any. `body_digest` is the SHA-256 hex of `body`, so a driver never
    /// re-hashes the same bytes.
    fn parse(
        &self,
        body: &[u8],
        revision: Option<&str>,
        body_digest: &str,
    ) -> Result<Candidate<T>, ContractError>;
}

/// What one refresh produced.
#[derive(Clone, Debug)]
pub enum FetchOutcome<T> {
    /// The conditional request answered `304` and the cached bytes are intact,
    /// or the fresh bytes are byte-identical to the cached ones. No candidate:
    /// the active generation stands.
    Unchanged,
    /// A full typed candidate, validated only structurally by the refresher.
    /// The store runs the provider validator and the anti-downgrade check.
    Candidate(Candidate<T>),
}

/// Result of a store-driven refresh round.
#[derive(Clone, Debug)]
pub enum RefreshReport<T> {
    /// The active generation stands; nothing was persisted.
    Unchanged { generation: String },
    /// A candidate was rejected. The reason is visible; the previous snapshot
    /// remains active.
    Rejected { reason: ContractError },
    /// A new generation was validated, persisted and published.
    Activated {
        generation: String,
        snapshot: Arc<super::Snapshot<T>>,
    },
}

/// Conditional acquisition for one contract, over a [`ContractTransport`].
pub struct ContractRefresher<T> {
    transport: Arc<dyn ContractTransport>,
    cache: Arc<TransportCache>,
    limits: ContractLimits,
    driver: Arc<dyn ContractDriver<T>>,
}

impl<T> ContractRefresher<T>
where
    T: serde::Serialize + Send + Sync + 'static,
{
    /// Build a refresher for `driver`.
    pub fn new(
        transport: Arc<dyn ContractTransport>,
        cache: Arc<TransportCache>,
        limits: ContractLimits,
        driver: Arc<dyn ContractDriver<T>>,
    ) -> Self {
        Self {
            transport,
            cache,
            limits,
            driver,
        }
    }

    /// The contract this refresher drives.
    pub fn contract_id(&self) -> ContractId {
        self.driver.contract_id()
    }

    /// The shared transport cache, so a store can inspect or warm it.
    pub fn cache(&self) -> &Arc<TransportCache> {
        &self.cache
    }

    /// Run one refresh round.
    ///
    /// Semantics:
    /// - `200` with bytes identical to a replayable cache entry → `Unchanged`,
    ///   and the cache validators are refreshed.
    /// - `200` with different bytes → `Candidate`.
    /// - `304` with intact cached bytes → `Unchanged`.
    /// - `304` without cached bytes → a single unconditional retry; if that also
    ///   answers `304`, [`ContractError::MissingCachedBytes`].
    /// - `404`/`410` → [`ContractError::SourceMissing`].
    /// - `429` → [`ContractError::RateLimited`] carrying `Retry-After`.
    /// - any other non-2xx, a timeout, a CDN error or invalid JSON →
    ///   the previous snapshot stays active and the error is reported.
    pub async fn refresh(&self) -> Result<FetchOutcome<T>, ContractError> {
        let descriptor = self.driver.source();
        let key =
            TransportCacheKey::new(&descriptor.url, &descriptor.accept, &descriptor.auth_scope);
        let cached = self.cache.get(&key);
        let request = self.build_request(&descriptor, cached.as_ref());
        let response = self.transport.fetch(&request).await?;

        match response.status {
            304 => {
                self.handle_not_modified(&key, &descriptor, cached, &response)
                    .await
            }
            200..=299 => self.handle_ok(&key, &descriptor, cached, &response).await,
            404 | 410 => Err(ContractError::SourceMissing {
                url: descriptor.url,
            }),
            429 => Err(ContractError::RateLimited {
                url: descriptor.url,
                retry_after: response.retry_after_secs(),
            }),
            status if (500..=599).contains(&status) => Err(ContractError::SourceUnavailable {
                url: descriptor.url,
                status,
            }),
            status => Err(ContractError::UnexpectedStatus {
                url: descriptor.url,
                status,
            }),
        }
    }

    /// Build the request for a round, adding conditional headers when the cache
    /// holds a replayable entry.
    fn build_request(
        &self,
        descriptor: &SourceDescriptor,
        cached: Option<&CacheEntry>,
    ) -> FetchRequest {
        let mut headers = descriptor.headers.clone();
        headers.push(("Accept".to_string(), descriptor.accept.clone()));
        if let Some(entry) = cached.filter(|entry| entry.is_replayable()) {
            headers.extend(entry.conditional_headers());
        }
        FetchRequest {
            key: TransportCacheKey::new(
                &descriptor.url,
                &descriptor.accept,
                &descriptor.auth_scope,
            ),
            method: "GET".to_string(),
            url: descriptor.url.clone(),
            headers,
        }
    }

    /// Handle a `304`.
    async fn handle_not_modified(
        &self,
        key: &TransportCacheKey,
        descriptor: &SourceDescriptor,
        cached: Option<CacheEntry>,
        response: &FetchResponse,
    ) -> Result<FetchOutcome<T>, ContractError> {
        if let Some(entry) = cached.filter(|entry| entry.body_matches_digest()) {
            let mut updated = entry;
            refresh_validators(&mut updated, response);
            updated.checked_at = Utc::now();
            self.cache.insert(updated);
            return Ok(FetchOutcome::Unchanged);
        }
        // No usable previous bytes: retry once unconditionally. A `304` to an
        // unconditional GET proves nothing about content, so refuse it.
        let retry = self.build_request(descriptor, None);
        let retried = self.transport.fetch(&retry).await?;
        match retried.status {
            200..=299 => self.handle_ok(key, descriptor, None, &retried).await,
            304 => Err(ContractError::MissingCachedBytes {
                url: descriptor.url.clone(),
            }),
            status => Err(status_error(&descriptor.url, status, &retried)),
        }
    }

    /// Handle a `2xx` body.
    async fn handle_ok(
        &self,
        key: &TransportCacheKey,
        descriptor: &SourceDescriptor,
        cached: Option<CacheEntry>,
        response: &FetchResponse,
    ) -> Result<FetchOutcome<T>, ContractError> {
        validate_source_body_size(response.body.len(), &self.limits)?;
        let digest = hex_digest(&response.body);
        if let Some(entry) = cached
            && entry.body_matches_digest()
            && entry.digest == digest
        {
            let mut updated = entry;
            refresh_validators(&mut updated, response);
            updated.checked_at = Utc::now();
            self.cache.insert(updated);
            return Ok(FetchOutcome::Unchanged);
        }
        self.record(key, response, digest.clone());
        let candidate =
            self.driver
                .parse(&response.body, descriptor.revision.as_deref(), &digest)?;
        Ok(FetchOutcome::Candidate(candidate))
    }

    /// Store the fresh bytes and validators for the next conditional request.
    fn record(&self, key: &TransportCacheKey, response: &FetchResponse, digest: String) {
        let entry = CacheEntry {
            key: key.clone(),
            etag: response.etag(),
            last_modified: response.last_modified(),
            checked_at: Utc::now(),
            digest,
            bytes: response.body.len(),
            body: Some(response.body.clone()),
        };
        self.cache.insert(entry);
    }
}

/// Replace the validators of a cache entry with the ones the server just sent,
/// keeping the held bytes and their digest.
fn refresh_validators(entry: &mut CacheEntry, response: &FetchResponse) {
    if let Some(etag) = response.etag() {
        entry.etag = Some(etag);
    }
    if let Some(last_modified) = response.last_modified() {
        entry.last_modified = Some(last_modified);
    }
}

/// Map a non-2xx, non-304 status to an error.
fn status_error(url: &str, status: u16, response: &FetchResponse) -> ContractError {
    match status {
        404 | 410 => ContractError::SourceMissing {
            url: url.to_string(),
        },
        429 => ContractError::RateLimited {
            url: url.to_string(),
            retry_after: response.retry_after_secs(),
        },
        status if (500..=599).contains(&status) => ContractError::SourceUnavailable {
            url: url.to_string(),
            status,
        },
        status => ContractError::UnexpectedStatus {
            url: url.to_string(),
            status,
        },
    }
}

/// The digest of a candidate's payload, so a driver can compare against a
/// cached digest without re-deriving it.
pub fn candidate_digest<T: serde::Serialize>(
    candidate: &Candidate<T>,
) -> Result<String, ContractError> {
    payload_digest(&candidate.payload)
}

/// Whether a response body looks like the JSON a driver expects, before it is
/// handed to a parser. An HTML error page served with `200` is refused here
/// rather than failing later inside a provider parser.
pub fn is_json_body(body: &[u8]) -> bool {
    let trimmed: Vec<u8> = body
        .iter()
        .copied()
        .skip_while(|b| b.is_ascii_whitespace())
        .collect();
    matches!(trimmed.first(), Some(b'{' | b'[')) && serde_json::from_slice::<Value>(body).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_contract::{
        ClientIdentity, IDENTITY_ORIGIN_OFFICIAL, SourceRef as Ref, candidate_payload_digest,
    };

    #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct DemoPayload {
        version: String,
    }

    /// A transport that answers one scripted response per request, in order,
    /// and records what it was asked for.
    #[derive(Default)]
    struct ScriptedTransport {
        responses: std::sync::Mutex<std::collections::VecDeque<FetchResponse>>,
        seen: std::sync::Mutex<Vec<FetchRequest>>,
    }

    impl ScriptedTransport {
        fn new(responses: Vec<FetchResponse>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses.into()),
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl ContractTransport for ScriptedTransport {
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
                .pop_front()
                .unwrap_or_else(|| FetchResponse {
                    status: 500,
                    headers: Vec::new(),
                    body: Vec::new(),
                });
            Box::pin(async move { Ok(response) })
        }
    }

    struct DemoDriver {
        revision: Option<&'static str>,
    }

    impl ContractDriver<DemoPayload> for DemoDriver {
        fn contract_id(&self) -> ContractId {
            ContractId::Codex
        }
        fn rule_set_id(&self) -> &'static str {
            "codex-rules"
        }
        fn extractor_version(&self) -> u32 {
            1
        }
        fn source(&self) -> SourceDescriptor {
            SourceDescriptor {
                url: "https://example.test/models.json".to_string(),
                accept: "application/json".to_string(),
                auth_scope: "public".to_string(),
                headers: Vec::new(),
                evidence: EvidenceKind::PackageManifest,
                revision: self.revision.map(str::to_string),
            }
        }
        fn parse(
            &self,
            body: &[u8],
            revision: Option<&str>,
            body_digest: &str,
        ) -> Result<Candidate<DemoPayload>, ContractError> {
            if !is_json_body(body) {
                return Err(ContractError::Encoding(std::sync::Arc::new(
                    serde_json::from_slice::<Value>(body).unwrap_err(),
                )));
            }
            let payload: DemoPayload = serde_json::from_slice(body)?;
            Ok(Candidate::without_catalog(
                ContractId::Codex,
                ClientIdentity::versioned(&payload.version, IDENTITY_ORIGIN_OFFICIAL),
                self.rule_set_id(),
                self.extractor_version(),
                vec![Ref::pinned(
                    "https://example.test/models.json",
                    revision.unwrap_or("unpinned"),
                    body_digest,
                    EvidenceKind::PackageManifest,
                )],
                payload,
            ))
        }
    }

    fn body(version: &str) -> Vec<u8> {
        serde_json::to_vec(&DemoPayload {
            version: version.to_string(),
        })
        .unwrap()
    }

    fn ok(body: &[u8], etag: Option<&str>) -> FetchResponse {
        FetchResponse {
            status: 200,
            headers: etag
                .map(|etag| ("etag".to_string(), etag.to_string()))
                .into_iter()
                .collect(),
            body: body.to_vec(),
        }
    }

    fn not_modified(etag: &str) -> FetchResponse {
        FetchResponse {
            status: 304,
            headers: vec![("etag".to_string(), etag.to_string())],
            body: Vec::new(),
        }
    }

    fn status(status: u16) -> FetchResponse {
        FetchResponse {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn refresher(
        responses: Vec<FetchResponse>,
        cache: Arc<TransportCache>,
        driver: Arc<dyn ContractDriver<DemoPayload>>,
        limits: ContractLimits,
    ) -> ContractRefresher<DemoPayload> {
        ContractRefresher::new(
            Arc::new(ScriptedTransport::new(responses)),
            cache,
            limits,
            driver,
        )
    }

    #[tokio::test]
    async fn a_first_fetch_yields_a_candidate_and_warms_the_cache() {
        let cache = Arc::new(TransportCache::new());
        let driver: Arc<dyn ContractDriver<DemoPayload>> = Arc::new(DemoDriver {
            revision: Some("rev-1"),
        });
        let outcome = refresher(
            vec![ok(&body("1.2.3"), Some("etag-1"))],
            Arc::clone(&cache),
            driver,
            ContractLimits::default(),
        )
        .refresh()
        .await
        .unwrap();
        assert!(matches!(outcome, FetchOutcome::Candidate(_)));
        assert_eq!(cache.len(), 1);
    }

    #[tokio::test]
    async fn a_second_fetch_with_a_matching_etag_is_unchanged() {
        let cache = Arc::new(TransportCache::new());
        let driver: Arc<dyn ContractDriver<DemoPayload>> = Arc::new(DemoDriver {
            revision: Some("rev-1"),
        });
        let limits = ContractLimits::default();
        refresher(
            vec![ok(&body("1.2.3"), Some("etag-1"))],
            Arc::clone(&cache),
            Arc::clone(&driver),
            limits,
        )
        .refresh()
        .await
        .unwrap();

        let outcome = refresher(
            vec![not_modified("etag-1")],
            Arc::clone(&cache),
            driver,
            limits,
        )
        .refresh()
        .await
        .unwrap();
        assert!(matches!(outcome, FetchOutcome::Unchanged));
    }

    #[tokio::test]
    async fn a_changed_body_yields_a_new_candidate() {
        let cache = Arc::new(TransportCache::new());
        let driver: Arc<dyn ContractDriver<DemoPayload>> = Arc::new(DemoDriver {
            revision: Some("rev-1"),
        });
        let limits = ContractLimits::default();
        refresher(
            vec![ok(&body("1.2.3"), Some("etag-1"))],
            Arc::clone(&cache),
            Arc::clone(&driver),
            limits,
        )
        .refresh()
        .await
        .unwrap();

        // The server answers our conditional request with a full body.
        let outcome = refresher(
            vec![ok(&body("1.2.4"), Some("etag-2"))],
            Arc::clone(&cache),
            driver,
            limits,
        )
        .refresh()
        .await
        .unwrap();
        match outcome {
            FetchOutcome::Candidate(candidate) => {
                assert_eq!(candidate.payload.version, "1.2.4");
            }
            FetchOutcome::Unchanged => panic!("expected a candidate"),
        }
    }

    #[tokio::test]
    async fn an_identical_body_with_a_new_etag_is_unchanged() {
        let cache = Arc::new(TransportCache::new());
        let driver: Arc<dyn ContractDriver<DemoPayload>> = Arc::new(DemoDriver {
            revision: Some("rev-1"),
        });
        let limits = ContractLimits::default();
        refresher(
            vec![ok(&body("1.2.3"), Some("etag-1"))],
            Arc::clone(&cache),
            Arc::clone(&driver),
            limits,
        )
        .refresh()
        .await
        .unwrap();

        let outcome = refresher(
            vec![ok(&body("1.2.3"), Some("etag-2"))],
            Arc::clone(&cache),
            driver,
            limits,
        )
        .refresh()
        .await
        .unwrap();
        assert!(matches!(outcome, FetchOutcome::Unchanged));
    }

    #[tokio::test]
    async fn a_conditional_request_carries_the_cached_validators() {
        let cache = Arc::new(TransportCache::new());
        let driver: Arc<dyn ContractDriver<DemoPayload>> = Arc::new(DemoDriver {
            revision: Some("rev-1"),
        });
        let transport = Arc::new(ScriptedTransport::new(vec![
            ok(&body("1.2.3"), Some("etag-1")),
            not_modified("etag-1"),
        ]));
        let refresher = ContractRefresher::new(
            Arc::clone(&transport) as Arc<dyn ContractTransport>,
            Arc::clone(&cache),
            ContractLimits::default(),
            Arc::clone(&driver),
        );
        refresher.refresh().await.unwrap();
        refresher.refresh().await.unwrap();

        let requests = transport.seen.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            !requests[0]
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("if-none-match")),
            "the first request must be unconditional"
        );
        let conditional = requests[1]
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("if-none-match"))
            .map(|(_, value)| value.clone());
        assert_eq!(conditional.as_deref(), Some("etag-1"));
        assert!(
            requests[1]
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("accept")),
            "the Accept header must be replayed"
        );
    }

    #[tokio::test]
    async fn a_304_without_cached_bytes_retries_then_rejects() {
        let cache = Arc::new(TransportCache::new());
        let driver: Arc<dyn ContractDriver<DemoPayload>> = Arc::new(DemoDriver {
            revision: Some("rev-1"),
        });
        let error = refresher(
            vec![not_modified("etag-1"), not_modified("etag-1")],
            cache,
            driver,
            ContractLimits::default(),
        )
        .refresh()
        .await
        .unwrap_err();
        assert!(matches!(error, ContractError::MissingCachedBytes { .. }));
    }

    #[tokio::test]
    async fn a_304_with_missing_cached_bytes_refetches() {
        let cache = Arc::new(TransportCache::new());
        let driver: Arc<dyn ContractDriver<DemoPayload>> = Arc::new(DemoDriver {
            revision: Some("rev-1"),
        });
        let limits = ContractLimits::default();
        refresher(
            vec![ok(&body("1.2.3"), Some("etag-1"))],
            Arc::clone(&cache),
            Arc::clone(&driver),
            limits,
        )
        .refresh()
        .await
        .unwrap();

        let key = TransportCacheKey::new(
            "https://example.test/models.json",
            "application/json",
            "public",
        );
        let mut entry = cache.get(&key).expect("cached entry");
        entry.body = None;
        cache.insert(entry);
        // 304, then a full body for the retry.
        let transport = Arc::new(ScriptedTransport::new(vec![
            not_modified("etag-1"),
            ok(&body("1.2.5"), Some("etag-3")),
        ]));
        let refresher = ContractRefresher::new(transport, cache, limits, Arc::clone(&driver));
        let outcome = refresher.refresh().await.unwrap();
        match outcome {
            FetchOutcome::Candidate(candidate) => {
                assert_eq!(candidate.payload.version, "1.2.5");
            }
            FetchOutcome::Unchanged => panic!("expected a refetch"),
        }
    }

    #[tokio::test]
    async fn a_repeated_304_after_a_refetch_is_refused() {
        let cache = Arc::new(TransportCache::new());
        let driver: Arc<dyn ContractDriver<DemoPayload>> = Arc::new(DemoDriver {
            revision: Some("rev-1"),
        });
        let limits = ContractLimits::default();
        refresher(
            vec![ok(&body("1.2.3"), Some("etag-1"))],
            Arc::clone(&cache),
            Arc::clone(&driver),
            limits,
        )
        .refresh()
        .await
        .unwrap();

        let key = TransportCacheKey::new(
            "https://example.test/models.json",
            "application/json",
            "public",
        );
        let mut entry = cache.get(&key).expect("cached entry");
        entry.body = None;
        cache.insert(entry);
        // A second 304 in the same round: there is nothing left to retry.
        let transport = Arc::new(ScriptedTransport::new(vec![
            not_modified("etag-1"),
            not_modified("etag-1"),
        ]));
        let refresher = ContractRefresher::new(transport, cache, limits, driver);
        let error = refresher.refresh().await.unwrap_err();
        assert!(matches!(error, ContractError::MissingCachedBytes { .. }));
    }

    #[tokio::test]
    async fn a_missing_source_does_not_evict_the_cache() {
        let cache = Arc::new(TransportCache::new());
        let driver: Arc<dyn ContractDriver<DemoPayload>> = Arc::new(DemoDriver {
            revision: Some("rev-1"),
        });
        let error = refresher(
            vec![status(404)],
            Arc::clone(&cache),
            driver,
            ContractLimits::default(),
        )
        .refresh()
        .await
        .unwrap_err();
        assert!(matches!(error, ContractError::SourceMissing { .. }));
        assert!(cache.is_empty());
    }

    #[tokio::test]
    async fn a_429_carries_retry_after() {
        let cache = Arc::new(TransportCache::new());
        let driver: Arc<dyn ContractDriver<DemoPayload>> = Arc::new(DemoDriver {
            revision: Some("rev-1"),
        });
        let error = refresher(
            vec![FetchResponse {
                status: 429,
                headers: vec![("retry-after".to_string(), "120".to_string())],
                body: Vec::new(),
            }],
            cache,
            driver,
            ContractLimits::default(),
        )
        .refresh()
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            ContractError::RateLimited {
                retry_after: Some(120),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn an_oversized_source_is_refused_before_parsing() {
        let limits = ContractLimits::tight();
        let cache = Arc::new(TransportCache::new());
        let driver: Arc<dyn ContractDriver<DemoPayload>> = Arc::new(DemoDriver {
            revision: Some("rev-1"),
        });
        let big = vec![b'x'; limits.source_bytes + 1];
        let error = refresher(vec![ok(&big, Some("etag-1"))], cache, driver, limits)
            .refresh()
            .await
            .unwrap_err();
        assert!(matches!(error, ContractError::SourceTooLarge { .. }));
    }

    #[tokio::test]
    async fn an_html_body_is_refused_as_a_non_json_source() {
        let cache = Arc::new(TransportCache::new());
        let driver: Arc<dyn ContractDriver<DemoPayload>> = Arc::new(DemoDriver {
            revision: Some("rev-1"),
        });
        let error = refresher(
            vec![ok(b"<html><body>502</body></html>", Some("etag-1"))],
            cache,
            driver,
            ContractLimits::default(),
        )
        .refresh()
        .await
        .unwrap_err();
        assert!(matches!(error, ContractError::Encoding(_)));
    }

    #[tokio::test]
    async fn a_server_error_does_not_evict_a_good_entry() {
        let cache = Arc::new(TransportCache::new());
        let driver: Arc<dyn ContractDriver<DemoPayload>> = Arc::new(DemoDriver {
            revision: Some("rev-1"),
        });
        let limits = ContractLimits::default();
        refresher(
            vec![ok(&body("1.2.3"), Some("etag-1"))],
            Arc::clone(&cache),
            Arc::clone(&driver),
            limits,
        )
        .refresh()
        .await
        .unwrap();
        assert_eq!(cache.len(), 1);

        let error = refresher(vec![status(503)], cache, driver, limits)
            .refresh()
            .await
            .unwrap_err();
        assert!(matches!(error, ContractError::SourceUnavailable { .. }));
    }

    #[test]
    fn the_cache_key_separates_url_accept_and_scope() {
        let a = TransportCacheKey::new("https://x.test/a", "application/json", "public");
        let b = TransportCacheKey::new("https://x.test/a", "application/json", "private");
        let c = TransportCacheKey::new("https://x.test/a", "text/html", "public");
        let d = TransportCacheKey::new("https://x.test/b", "application/json", "public");
        let digests = [a.digest(), b.digest(), c.digest(), d.digest()];
        let unique: std::collections::BTreeSet<String> = digests.iter().cloned().collect();
        assert_eq!(unique.len(), 4);
        assert!(a.digest().bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn the_cache_key_does_not_fold_case() {
        let lower = TransportCacheKey::new("https://x.test/a", "application/json", "public");
        let upper_path = TransportCacheKey::new("https://x.test/A", "application/json", "public");
        let upper_accept = TransportCacheKey::new("https://x.test/a", "Application/JSON", "public");
        let upper_scope = TransportCacheKey::new("https://x.test/a", "application/json", "Public");
        assert_ne!(
            lower.digest(),
            upper_path.digest(),
            "a path is case-sensitive"
        );
        assert_ne!(
            lower.digest(),
            upper_accept.digest(),
            "an Accept value is not folded"
        );
        assert_ne!(
            lower.digest(),
            upper_scope.digest(),
            "a scope label is not folded"
        );
    }

    #[test]
    fn a_cache_entry_replays_only_when_its_bytes_match() {
        let payload = body("1.2.3");
        let entry = CacheEntry {
            key: TransportCacheKey::new("https://x.test/a", "application/json", "public"),
            etag: Some("etag-1".to_string()),
            last_modified: None,
            checked_at: Utc::now(),
            digest: hex_digest(&payload),
            bytes: payload.len(),
            body: Some(payload.clone()),
        };
        assert!(entry.is_replayable());
        assert!(entry.body_matches_digest());
        assert_eq!(
            entry.conditional_headers(),
            vec![("If-None-Match".to_string(), "etag-1".to_string())]
        );

        let mut broken = entry.clone();
        broken.digest = "0".repeat(64);
        assert!(!broken.body_matches_digest());
        let mut nobody = entry;
        nobody.body = None;
        assert!(!nobody.is_replayable());
    }

    #[test]
    fn the_transport_cache_inserts_gets_touches_and_invalidates() {
        let cache = TransportCache::new();
        assert!(cache.is_empty());
        let key = TransportCacheKey::new("https://x.test/a", "application/json", "public");
        cache.insert(CacheEntry {
            key: key.clone(),
            etag: None,
            last_modified: None,
            checked_at: Utc::now(),
            digest: "a".repeat(64),
            bytes: 0,
            body: None,
        });
        assert_eq!(cache.len(), 1);
        assert!(cache.get(&key).is_some());
        cache.touch(&key, Utc::now());
        cache.invalidate(&key);
        assert!(cache.is_empty());
    }

    #[test]
    fn the_candidate_digest_matches_the_payload_helper() {
        let candidate = Candidate::<DemoPayload>::without_catalog(
            ContractId::Codex,
            ClientIdentity::versioned("1.0.0", IDENTITY_ORIGIN_OFFICIAL),
            "codex-rules",
            1,
            vec![Ref::pinned(
                "https://example.test/x.json",
                "rev",
                "d".repeat(64),
                EvidenceKind::PackageManifest,
            )],
            DemoPayload {
                version: "1.0.0".to_string(),
            },
        );
        assert_eq!(
            candidate_digest(&candidate).unwrap(),
            candidate_payload_digest(&candidate).unwrap()
        );
    }

    #[test]
    fn json_detection_rejects_html_and_truncated_json() {
        assert!(is_json_body(b"  {\"a\":1}"));
        assert!(is_json_body(b"[1,2]"));
        assert!(!is_json_body(b"<html>"));
        assert!(!is_json_body(b"{\"a\":"));
        assert!(!is_json_body(b""));
    }
}
