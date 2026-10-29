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
