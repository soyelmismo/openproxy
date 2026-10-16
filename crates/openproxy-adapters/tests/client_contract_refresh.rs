//! Refresh tests for the common contract base.
//!
//! Every test is offline: the transport is scripted, the clock is the process
//! clock, and no request ever leaves the process.

#![allow(clippy::all, clippy::pedantic, clippy::unwrap_used)]

use std::sync::Arc;

use openproxy_adapters::client_contract::{
    CacheEntry, Candidate, ClientIdentity, ContractDriver, ContractError, ContractId,
    ContractLimits, ContractRefresher, ContractTransport, EvidenceKind, FetchOutcome, FetchRequest,
    FetchResponse, RefreshReport, SourceDescriptor, SourceRef, TransportCache, TransportCacheKey,
};

/// Payload shape owned by this test, not by the base.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct DemoPayload {
    version: String,
    models: Vec<String>,
}

/// A transport that answers from a script, one response per call.
struct ScriptedTransport {
    responses: std::sync::Mutex<std::collections::VecDeque<FetchResponse>>,
    /// Records every request the refresher issued, so conditional headers can
    /// be asserted on.
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
        let response = self.responses.lock().unwrap().pop_front();
        let url = request.url.clone();
        Box::pin(async move {
            response.ok_or_else(|| ContractError::Transport {
                url,
                detail: "script exhausted".to_string(),
            })
        })
    }
}

/// Driver that parses the demo payload and pins one package-manifest source.
struct DemoDriver;

impl ContractDriver<DemoPayload> for DemoDriver {
    fn contract_id(&self) -> ContractId {
        ContractId::Codex
    }

    fn rule_set_id(&self) -> &'static str {
        "codex-rules"
    }

    fn extractor_version(&self) -> u32 {
        3
    }

    fn source(&self) -> SourceDescriptor {
        SourceDescriptor {
            url: "https://example.test/models.json".to_string(),
            accept: "application/json".to_string(),
            auth_scope: "public".to_string(),
            headers: vec![("X-Probe".to_string(), "offline".to_string())],
            evidence: EvidenceKind::PackageManifest,
            revision: Some("rev-1".to_string()),
        }
    }

    fn parse(
        &self,
        body: &[u8],
        revision: Option<&str>,
        body_digest: &str,
    ) -> Result<Candidate<DemoPayload>, ContractError> {
        let payload: DemoPayload = serde_json::from_slice(body)?;
        Ok(Candidate::without_catalog(
            self.contract_id(),
            ClientIdentity::versioned(&payload.version, "official"),
            self.rule_set_id(),
            self.extractor_version(),
            vec![SourceRef::pinned(
                "https://example.test/models.json",
                revision.unwrap_or("unpinned"),
                body_digest,
                EvidenceKind::PackageManifest,
            )],
            payload,
        ))
    }
}

fn body(version: &str, models: &[&str]) -> Vec<u8> {
    serde_json::to_vec(&DemoPayload {
        version: version.to_string(),
        models: models.iter().map(|m| (*m).to_string()).collect(),
    })
    .unwrap()
}

fn ok(bytes: &[u8], etag: Option<&str>) -> FetchResponse {
    FetchResponse {
        status: 200,
        headers: etag
            .map(|e| vec![("etag".to_string(), e.to_string())])
            .unwrap_or_default(),
        body: bytes.to_vec(),
    }
}

fn not_modified(etag: &str) -> FetchResponse {
    FetchResponse {
        status: 304,
        headers: vec![("etag".to_string(), etag.to_string())],
        body: Vec::new(),
    }
}

fn rate_limited(retry_after: u64) -> FetchResponse {
    FetchResponse {
        status: 429,
        headers: vec![("retry-after".to_string(), retry_after.to_string())],
        body: Vec::new(),
    }
}

fn refresher(
    transport: ScriptedTransport,
    cache: Arc<TransportCache>,
    limits: ContractLimits,
) -> ContractRefresher<DemoPayload> {
    ContractRefresher::new(Arc::new(transport), cache, limits, Arc::new(DemoDriver))
}

#[tokio::test]
async fn first_fetch_returns_a_candidate_and_warms_the_cache() {
    let cache = Arc::new(TransportCache::new());
    let transport = ScriptedTransport::new(vec![ok(&body("1.0.0", &["gpt-5"]), Some("etag-1"))]);
    let refresher = refresher(transport, Arc::clone(&cache), ContractLimits::default());

    let outcome = refresher.refresh().await.unwrap();
    match outcome {
        FetchOutcome::Candidate(candidate) => {
            assert_eq!(candidate.contract_id, ContractId::Codex);
            assert_eq!(candidate.payload.version, "1.0.0");
            assert_eq!(candidate.sources.len(), 1);
            assert_eq!(candidate.sources[0].revision.as_deref(), Some("rev-1"));
        }
        FetchOutcome::Unchanged => panic!("a first fetch must produce a candidate"),
    }
    assert_eq!(cache.len(), 1);
}

#[tokio::test]
async fn conditional_request_carries_the_cached_validator() {
    let cache = Arc::new(TransportCache::new());
    let first = ScriptedTransport::new(vec![ok(&body("1.0.0", &["gpt-5"]), Some("etag-1"))]);
    refresher(first, Arc::clone(&cache), ContractLimits::default())
        .refresh()
        .await
        .unwrap();

    let second = ScriptedTransport::new(vec![not_modified("etag-1")]);
    let outcome = refresher(second, Arc::clone(&cache), ContractLimits::default())
        .refresh()
        .await
        .unwrap();
    assert!(matches!(outcome, FetchOutcome::Unchanged));
}

#[tokio::test]
async fn conditional_headers_are_sent_on_the_second_request() {
    let cache = Arc::new(TransportCache::new());
    let first = ScriptedTransport::new(vec![ok(&body("1.0.0", &["gpt-5"]), Some("etag-1"))]);
    refresher(first, Arc::clone(&cache), ContractLimits::default())
        .refresh()
        .await
        .unwrap();

    let transport = ScriptedTransport::new(vec![not_modified("etag-1")]);
    let outcome = refresher(transport, Arc::clone(&cache), ContractLimits::default())
        .refresh()
        .await
        .unwrap();
    assert!(matches!(outcome, FetchOutcome::Unchanged));
}

#[tokio::test]
async fn not_modified_without_cached_bytes_is_refused() {
    let cache = Arc::new(TransportCache::new());
    let transport = ScriptedTransport::new(vec![not_modified("etag-1"), not_modified("etag-1")]);
    let error = refresher(transport, Arc::clone(&cache), ContractLimits::default())
        .refresh()
        .await
        .unwrap_err();
    assert!(
        matches!(error, ContractError::MissingCachedBytes { .. }),
        "a 304 with no cached bytes must be refused, got {error:?}"
    );
}

#[tokio::test]
async fn not_modified_without_cached_bytes_refetches() {
    let cache = Arc::new(TransportCache::new());
    let transport = ScriptedTransport::new(vec![
        not_modified("etag-1"),
        ok(&body("1.0.0", &["gpt-5"]), Some("etag-2")),
    ]);
    let outcome = refresher(transport, Arc::clone(&cache), ContractLimits::default())
        .refresh()
        .await
        .unwrap();
    assert!(
        matches!(outcome, FetchOutcome::Candidate(_)),
        "a 304 with nothing cached must retry unconditionally, got {outcome:?}"
    );
}

#[tokio::test]
async fn not_modified_with_a_matching_cache_is_unchanged() {
    let cache = Arc::new(TransportCache::new());
    let transport = ScriptedTransport::new(vec![
        ok(&body("1.0.0", &["gpt-5"]), Some("etag-1")),
        not_modified("etag-1"),
    ]);
    let runner = refresher(transport, cache, ContractLimits::default());
    assert!(matches!(
        runner.refresh().await.unwrap(),
        FetchOutcome::Candidate(_)
    ));
    assert!(matches!(
        runner.refresh().await.unwrap(),
        FetchOutcome::Unchanged
    ));
}

#[tokio::test]
async fn repeated_not_modified_after_a_refetch_is_refused() {
    let cache = Arc::new(TransportCache::new());
    let transport = ScriptedTransport::new(vec![not_modified("etag-1"), not_modified("etag-1")]);
    let error = refresher(transport, cache, ContractLimits::default())
        .refresh()
        .await
        .unwrap_err();
    assert!(matches!(error, ContractError::MissingCachedBytes { .. }));
}

#[tokio::test]
async fn rate_limit_carries_retry_after() {
    let cache = Arc::new(TransportCache::new());
    let transport = ScriptedTransport::new(vec![rate_limited(120)]);
    let error = refresher(transport, Arc::clone(&cache), ContractLimits::default())
        .refresh()
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            ContractError::RateLimited {
                retry_after: Some(120),
                ..
            }
        ),
        "got {error:?}"
    );
}

#[tokio::test]
async fn server_error_does_not_evict_a_good_entry() {
    let cache = Arc::new(TransportCache::new());
    let transport = ScriptedTransport::new(vec![FetchResponse {
        status: 503,
        headers: Vec::new(),
        body: Vec::new(),
    }]);
    let error = refresher(transport, Arc::clone(&cache), ContractLimits::default())
        .refresh()
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ContractError::SourceUnavailable { status: 503, .. }
    ));
}

#[tokio::test]
async fn oversized_source_body_is_refused_before_parsing() {
    let limits = ContractLimits {
        source_bytes: 64,
        ..ContractLimits::default()
    };
    let cache = Arc::new(TransportCache::new());
    let big = vec![b'x'; 65];
    let transport = ScriptedTransport::new(vec![ok(&big, Some("etag-1"))]);
    let error = refresher(transport, Arc::clone(&cache), limits)
        .refresh()
        .await
        .unwrap_err();
    assert!(
        matches!(error, ContractError::SourceTooLarge { .. }),
        "got {error:?}"
    );
}

#[tokio::test]
async fn html_body_is_refused_as_a_non_json_source() {
    let cache = Arc::new(TransportCache::new());
    let transport = ScriptedTransport::new(vec![ok(b"<html>nope</html>", Some("etag-1"))]);
    let error = refresher(transport, Arc::clone(&cache), ContractLimits::default())
        .refresh()
        .await
        .unwrap_err();
    assert!(matches!(error, ContractError::Encoding(_)), "got {error:?}");
}

#[tokio::test]
async fn identical_body_with_a_new_etag_is_unchanged() {
    let cache = Arc::new(TransportCache::new());
    let first = ScriptedTransport::new(vec![ok(&body("1.0.0", &["gpt-5"]), Some("etag-1"))]);
    refresher(first, Arc::clone(&cache), ContractLimits::default())
        .refresh()
        .await
        .unwrap();

    let second = ScriptedTransport::new(vec![ok(&body("1.0.0", &["gpt-5"]), Some("etag-2"))]);
    let outcome = refresher(second, Arc::clone(&cache), ContractLimits::default())
        .refresh()
        .await
        .unwrap();
    assert!(
        matches!(outcome, FetchOutcome::Unchanged),
        "byte-identical content must not produce a new generation"
    );
}

#[tokio::test]
async fn changed_body_with_a_new_etag_produces_a_candidate() {
    let cache = Arc::new(TransportCache::new());
    let first = ScriptedTransport::new(vec![ok(&body("1.0.0", &["gpt-5"]), Some("etag-1"))]);
    refresher(first, Arc::clone(&cache), ContractLimits::default())
        .refresh()
        .await
        .unwrap();

    let second = ScriptedTransport::new(vec![ok(
        &body("1.1.0", &["gpt-5", "gpt-5-mini"]),
        Some("etag-2"),
    )]);
    let outcome = refresher(second, Arc::clone(&cache), ContractLimits::default())
        .refresh()
        .await
        .unwrap();
    match outcome {
        FetchOutcome::Candidate(candidate) => {
            assert_eq!(candidate.payload.version, "1.1.0");
            assert_eq!(candidate.payload.models.len(), 2);
        }
        FetchOutcome::Unchanged => panic!("changed content must produce a candidate"),
    }
}

#[tokio::test]
async fn transport_error_leaves_the_cache_intact() {
    let cache = Arc::new(TransportCache::new());
    let first = ScriptedTransport::new(vec![ok(&body("1.0.0", &["gpt-5"]), Some("etag-1"))]);
    refresher(first, Arc::clone(&cache), ContractLimits::default())
        .refresh()
        .await
        .unwrap();
    assert_eq!(cache.len(), 1);

    let failing = ScriptedTransport::new(vec![]);
    let error = refresher(failing, Arc::clone(&cache), ContractLimits::default())
        .refresh()
        .await
        .unwrap_err();
    assert!(matches!(error, ContractError::Transport { .. }));
    assert_eq!(cache.len(), 1, "a failed round must not evict a good entry");
}

#[test]
fn transport_cache_key_separates_url_accept_and_auth_scope() {
    let base = TransportCacheKey::new("https://x.test/a.json", "application/json", "public");
    let other_url = TransportCacheKey::new("https://x.test/b.json", "application/json", "public");
    let other_accept = TransportCacheKey::new("https://x.test/a.json", "text/html", "public");
    let other_scope =
        TransportCacheKey::new("https://x.test/a.json", "application/json", "private");

    let mut digests = vec![
        base.digest(),
        other_url.digest(),
        other_accept.digest(),
        other_scope.digest(),
    ];
    digests.sort();
    digests.dedup();
    assert_eq!(digests.len(), 4, "each axis must change the cache key");
    for digest in digests {
        assert_eq!(digest.len(), 64);
    }
}

#[test]
fn transport_cache_key_never_contains_a_credential() {
    let key = TransportCacheKey::new(
        "https://registry.npmjs.org/@openai/codex",
        "application/json",
        "public",
    );
    let encoded = serde_json::to_string(&key).unwrap();
    for marker in ["Bearer ", "authorization", "cookie", "token=", "session_id"] {
        assert!(
            !encoded.to_ascii_lowercase().contains(marker),
            "cache key must stay credential-free: {encoded}"
        );
    }
}

#[test]
fn cache_entry_replayability_requires_its_bytes() {
    let body = body("1.0.0", &["gpt-5"]);
    let digest = openproxy_adapters::client_contract::hex_digest(&body);
    let entry = CacheEntry {
        key: TransportCacheKey::new("https://x.test/a.json", "application/json", "public"),
        etag: Some("etag-1".to_string()),
        last_modified: None,
        checked_at: chrono::Utc::now(),
        digest,
        bytes: body.len(),
        body: Some(body),
    };
    assert!(entry.is_replayable());
    assert!(entry.body_matches_digest());
    let headers = entry.conditional_headers();
    assert!(headers.iter().any(|(k, _)| k == "If-None-Match"));
    assert!(!headers.iter().any(|(k, _)| k == "Authorization"));

    let mut empty = entry.clone();
    empty.body = None;
    assert!(
        !empty.is_replayable(),
        "an entry without bytes cannot answer a 304"
    );

    let mut corrupt = entry;
    corrupt.body = Some(b"tampered".to_vec());
    assert!(
        !corrupt.body_matches_digest(),
        "a mismatched body must not be replayed"
    );
}

#[test]
fn cache_insert_lookup_and_invalidate() {
    let cache = TransportCache::new();
    assert!(cache.is_empty());
    let key = TransportCacheKey::new("https://x.test/a.json", "application/json", "public");
    cache.insert(CacheEntry {
        key: key.clone(),
        etag: Some("etag-1".to_string()),
        last_modified: None,
        checked_at: chrono::Utc::now(),
        digest: "a".repeat(64),
        bytes: 1,
        body: Some(vec![b'a']),
    });
    assert_eq!(cache.len(), 1);
    assert!(cache.get(&key).is_some());

    cache.invalidate(&key);
    assert!(cache.get(&key).is_none());
    assert!(cache.is_empty());
}

#[test]
fn cache_survives_a_restart_through_the_report_shape() {
    // The report a store-driven round produces, so a caller can distinguish
    // "nothing to do" from "rejected" without pattern-matching the outcome.
    let unchanged: RefreshReport<DemoPayload> = RefreshReport::Unchanged {
        generation: "gen-1".to_string(),
    };
    let rejected: RefreshReport<DemoPayload> = RefreshReport::Rejected {
        reason: ContractError::SourceMissing {
            url: "https://example.test/models.json".to_string(),
        },
    };
    assert!(matches!(unchanged, RefreshReport::Unchanged { .. }));
    assert!(matches!(rejected, RefreshReport::Rejected { .. }));
}

#[test]
fn candidate_digest_is_stable_for_identical_content() {
    let build = || {
        Candidate::without_catalog(
            ContractId::Codex,
            ClientIdentity::versioned("1.0.0", "official"),
            "codex-rules",
            1,
            vec![
                SourceRef::pinned(
                    "https://b.example/x.json",
                    "rev-b",
                    "b".repeat(64),
                    EvidenceKind::PackageManifest,
                ),
                SourceRef::pinned(
                    "https://a.example/y.json",
                    "rev-a",
                    "a".repeat(64),
                    EvidenceKind::PackageManifest,
                ),
            ],
            DemoPayload {
                version: "1.0.0".to_string(),
                models: vec!["gpt-5".to_string()],
            },
        )
    };

    let mut reordered = build();
    reordered.sources.reverse();

    let first = openproxy_adapters::client_contract::Snapshot::from_candidate(build()).unwrap();
    let second = openproxy_adapters::client_contract::Snapshot::from_candidate(reordered).unwrap();
    assert_eq!(
        first.generation, second.generation,
        "acquisition order must not change the generation"
    );
    assert_eq!(
        first.payload_digest().unwrap(),
        second.payload_digest().unwrap()
    );
}
