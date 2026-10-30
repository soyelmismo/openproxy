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
            status => Err(status_error(&descriptor.url, status, &response)),
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
            return Ok(self.serve_unchanged(entry, response));
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
            return Ok(self.serve_unchanged(entry, response));
        }
        self.record(key, response, digest.clone());
        let candidate =
            self.driver
                .parse(&response.body, descriptor.revision.as_deref(), &digest)?;
        Ok(FetchOutcome::Candidate(candidate))
    }

    /// Refresh the held entry's validators and answer `Unchanged`: the server
    /// confirmed the cached bytes still stand.
    fn serve_unchanged(&self, mut entry: CacheEntry, response: &FetchResponse) -> FetchOutcome<T> {
        refresh_validators(&mut entry, response);
        entry.checked_at = Utc::now();
        self.cache.insert(entry);
        FetchOutcome::Unchanged
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
mod tests;
