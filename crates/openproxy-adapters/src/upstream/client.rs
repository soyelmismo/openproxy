//! The `UpstreamClient` — the hyper-based replacement for the
//! UpstreamClient-based `UpstreamClient` used by the chat pipeline.
//!
//! See the module-level docs in `mod.rs` for the full architecture;
//! this file is the implementation.

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use http::{HeaderMap, Method, Request, Uri};
use http_body_util::Full;

use super::cancel::CancellationToken;
use super::conn_pool::{HostKey, Scheme, UpstreamConnectionPool as Pool};
use super::error::{UpstreamError, UpstreamResult};
use super::phases::{ResolvedPhaseDeadlines, UpstreamPhase};
use super::profile::TimeoutProfile;
use super::response::{UpstreamBodyStream, UpstreamResponse};

#[cfg(feature = "upstream-hyper")]
use super::connector::{CALL_PROXY, CALL_TIMEOUTS, PhasedConnector, PhasedTimeouts, phased_phase};
#[cfg(feature = "upstream-hyper")]
use hyper_util::client::legacy::Client as HyperClient;
#[cfg(feature = "upstream-hyper")]
use hyper_util::client::legacy::connect::Connection as HyperConnection;
#[cfg(feature = "upstream-hyper")]
use hyper_util::rt::TokioExecutor;

// -----------------------------------------------------------------------
// UpstreamRequest
// -----------------------------------------------------------------------

/// Caller-supplied request shape. The client only needs a URL, method,
/// headers, and a body. The body is bounded to keep the simple
/// non-streaming path easy; streaming bodies are a Gate-4 concern.
#[derive(Debug, Clone)]
pub struct UpstreamRequest {
    pub method: Method,
    pub url: String,
    pub headers: HeaderMap,
    pub body: Option<Bytes>,
    /// When `false`, the body-chunk gap timeout (idle_chunk_ms) is NOT
    /// applied to the response body. Only `total_ms` bounds the body
    /// read. Set to `false` for non-streaming requests where the LLM
    /// generates the full response server-side before sending anything.
    /// Default: `true` (streaming).
    pub is_streaming: bool,
    pub proxy: Option<String>,
    pub proxy_status: Option<String>,
}

impl UpstreamRequest {
    /// Build a simple GET with no headers / body.
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: Method::GET,
            url: url.into(),
            headers: HeaderMap::new(),
            body: None,
            is_streaming: true,
            proxy: None,
            proxy_status: None,
        }
    }

    /// Build a simple DELETE with no headers / body.
    pub fn delete(url: impl Into<String>) -> Self {
        Self {
            method: Method::DELETE,
            url: url.into(),
            headers: HeaderMap::new(),
            body: None,
            is_streaming: true,
            proxy: None,
            proxy_status: None,
        }
    }

    /// Build a POST with a JSON body and a `Content-Type: application/json` header.
    pub fn post_json(url: impl Into<String>, body: Bytes) -> Self {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        Self {
            method: Method::POST,
            url: url.into(),
            headers,
            body: Some(body),
            is_streaming: true,
            proxy: None,
            proxy_status: None,
        }
    }

    /// Create a POST request with a custom Content-Type (for
    /// `multipart/form-data`). The caller must build the multipart
    /// body themselves and pass a `Content-Type` value that includes
    /// the boundary (e.g.
    /// `multipart/form-data; boundary=----WebKitFormBoundary...`).
    ///
    /// Used by the audio-transcription handler to forward the
    /// pre-built multipart body to the upstream's
    /// `/audio/transcriptions` endpoint. The hyper-based
    /// `UpstreamClient` does not need to know about the multipart
    /// shape — it just ships the bytes through with the supplied
    /// Content-Type.
    pub fn post_multipart(url: impl Into<String>, content_type: &str, body: Bytes) -> Self {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_str(content_type)
                .unwrap_or_else(|_| http::HeaderValue::from_static("multipart/form-data")),
        );
        Self {
            method: Method::POST,
            url: url.into(),
            headers,
            body: Some(body),
            // Non-streaming: the upstream builds the full transcription
            // server-side before sending the response. Disabling the
            // body-chunk gap timeout keeps the (potentially long)
            // transcription from being killed by an idle-chunk watchdog.
            is_streaming: false,
            proxy: None,
            proxy_status: None,
        }
    }
}

// -----------------------------------------------------------------------
// UpstreamClient
// -----------------------------------------------------------------------

/// A hyper-based HTTP client with per-phase timeouts and a per-host
/// connection pool.
///
/// The struct is private; users get an `Arc<UpstreamClient>` from
/// `new()`. Internally we keep the hyper `Client` and the per-host
/// pool (which is just the observability layer over the hyper
/// client's own internal pool).
pub trait UpstreamTransport: Send + Sync + std::fmt::Debug {
    fn send_request(
        &self,
        req: Request<Full<Bytes>>,
        connector_timeouts: PhasedTimeouts,
        proxy_url: Option<String>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<http::Response<hyper::body::Incoming>, UpstreamError>,
                > + Send
                + '_,
        >,
    >;

    fn phase_hint(&self) -> Option<UpstreamPhase>;
}

/// A hyper-based HTTP client with per-phase timeouts and a per-host
/// connection pool.
///
/// The struct is private; users get an `Arc<UpstreamClient>` from
/// `new()`. Internally we keep the hyper `Client` and the per-host
/// pool (which is just the observability layer over the hyper
/// client's own internal pool).
pub struct UpstreamClient {
    pool: Pool,
    #[cfg(feature = "upstream-hyper")]
    transport: Arc<dyn UpstreamTransport>,
}

#[derive(Debug)]
struct ProductionTransport {
    hyper: HyperClient<PhasedConnector, Full<Bytes>>,
}

impl UpstreamTransport for ProductionTransport {
    fn send_request(
        &self,
        req: Request<Full<Bytes>>,
        connector_timeouts: PhasedTimeouts,
        proxy_url: Option<String>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<http::Response<hyper::body::Incoming>, UpstreamError>,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let fut = async move {
                self.hyper.request(req).await.map_err(|e| {
                    if let Some(up_err) = hyper_source_connector_error(&e) {
                        return up_err;
                    }
                    let phase = hyper_source_phase(&e);
                    let msg = format_hyper_error(&e);
                    match phase {
                        Some(p) => UpstreamError::Timeout(p),
                        None => UpstreamError::Http(msg),
                    }
                })
            };
            let fut = CALL_PROXY.scope(proxy_url, fut);
            let fut = CALL_TIMEOUTS.scope(connector_timeouts, fut);
            fut.await
        })
    }

    fn phase_hint(&self) -> Option<UpstreamPhase> {
        None
    }
}

#[cfg(feature = "upstream-hyper")]
struct TestTransport<C> {
    hyper: HyperClient<C, Full<Bytes>>,
    phase_hint: Option<UpstreamPhase>,
}

impl<C, T> std::fmt::Debug for TestTransport<C>
where
    C: tower_service::Service<
            Uri,
            Response = T,
            Error = Box<dyn std::error::Error + Send + Sync>,
            Future: Send + Unpin + 'static,
        > + Send
        + Sync
        + 'static,
    T: hyper::rt::Read + hyper::rt::Write + HyperConnection + Unpin + Send + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestTransport")
            .field("phase_hint", &self.phase_hint)
            .finish_non_exhaustive()
    }
}

impl<C, T> UpstreamTransport for TestTransport<C>
where
    C: tower_service::Service<
            Uri,
            Response = T,
            Error = Box<dyn std::error::Error + Send + Sync>,
            Future: Send + Unpin + 'static,
        > + Send
        + Sync
        + Clone
        + 'static,
    T: hyper::rt::Read + hyper::rt::Write + HyperConnection + Unpin + Send + 'static,
{
    fn send_request(
        &self,
        req: Request<Full<Bytes>>,
        connector_timeouts: PhasedTimeouts,
        proxy_url: Option<String>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<http::Response<hyper::body::Incoming>, UpstreamError>,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let fut = async move {
                self.hyper.request(req).await.map_err(|e| {
                    if let Some(phase) = hyper_source_phase(&e) {
                        return UpstreamError::Timeout(phase);
                    }
                    if e.is_connect() {
                        UpstreamError::Connection(e.to_string())
                    } else {
                        UpstreamError::Http(e.to_string())
                    }
                })
            };
            let fut = CALL_PROXY.scope(proxy_url, fut);
            let fut = CALL_TIMEOUTS.scope(connector_timeouts, fut);
            fut.await
        })
    }

    fn phase_hint(&self) -> Option<UpstreamPhase> {
        self.phase_hint
    }
}

impl UpstreamClient {
    /// Build a new client with the default connector (HTTPS via rustls
    /// with safe defaults, HTTP plain). Returns an `Arc<UpstreamClient>`
    /// per the spec API.
    pub fn new() -> Arc<Self> {
        #[cfg(feature = "upstream-hyper")]
        {
            let connector = PhasedConnector::with_defaults();
            let hyper: HyperClient<PhasedConnector, Full<Bytes>> =
                HyperClient::builder(TaskLocalExecutor)
                    .pool_max_idle_per_host(8)
                    .pool_idle_timeout(std::time::Duration::from_secs(20))
                    .build(connector);
            let pool = Pool::new();
            spawn_eviction_loop(Pool::clone(&pool));
            Arc::new(Self {
                pool,
                transport: Arc::new(ProductionTransport { hyper }),
            })
        }
        #[cfg(not(feature = "upstream-hyper"))]
        {
            let pool = Pool::new();
            spawn_eviction_loop(Pool::clone(&pool));
            Arc::new(Self { pool })
        }
    }

    /// Test-only: build a client with a custom connector. The
    /// connector must be `Clone` and implement
    /// `tower_service::Service<Uri>` with the hyper-util connect
    /// future type. The supplied `phase_hint` is consulted when a
    /// timeout fires during the connect/headers phase: if set, the
    /// returned error is `Timeout(phase_hint)`; if not, it falls
    /// back to `Timeout(Headers)`.
    #[cfg(feature = "upstream-hyper")]
    pub fn for_test_with_connector<C, T>(
        connector: C,
        phase_hint: Option<UpstreamPhase>,
    ) -> Arc<Self>
    where
        C: tower_service::Service<
                Uri,
                Response = T,
                Error = Box<dyn std::error::Error + Send + Sync>,
                Future: Send + Unpin + 'static,
            > + Send
            + Sync
            + Clone
            + 'static,
        T: hyper::rt::Read + hyper::rt::Write + HyperConnection + Unpin + Send + 'static,
    {
        let hyper: HyperClient<C, Full<Bytes>> = HyperClient::builder(TokioExecutor::new())
            .pool_max_idle_per_host(0)
            .build(connector);
        let transport = Arc::new(TestTransport { hyper, phase_hint });
        Arc::new(Self {
            pool: Pool::new(),
            transport,
        })
    }

    /// Get a handle to the connection pool (for tests / metrics).
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Send a request. The pool counter is updated on success.
    pub async fn call(
        self: &Arc<Self>,
        spec: UpstreamRequest,
        profile: TimeoutProfile,
        cancel: CancellationToken,
    ) -> UpstreamResult<UpstreamResponse> {
        #[cfg(feature = "upstream-hyper")]
        {
            self.call_inner(spec, profile, cancel).await
        }
        #[cfg(not(feature = "upstream-hyper"))]
        {
            let _ = (spec, profile, cancel);
            Err(UpstreamError::Invalid(
                "upstream-hyper feature is disabled in this build".to_string(),
            ))
        }
    }

    #[cfg(feature = "upstream-hyper")]
    async fn call_inner(
        self: &Arc<Self>,
        spec: UpstreamRequest,
        profile: TimeoutProfile,
        cancel: CancellationToken,
    ) -> UpstreamResult<UpstreamResponse> {
        let start = Instant::now();
        let timeouts = profile.resolve();
        let deadlines = ResolvedPhaseDeadlines::from_profile(start, &timeouts);

        if cancel.is_cancelled() {
            return Err(UpstreamError::Cancel);
        }

        let is_streaming = spec.is_streaming;
        let proxy_url = spec.proxy.clone();
        let (_uri, host_key, host) = build_host_key_and_uri(&spec.url)?;
        let request = build_hyper_request(spec)?;

        let pool = Pool::clone(&self.pool);
        let cancel_for_send = CancellationToken::clone(&cancel);
        let transport = Arc::clone(&self.transport);
        let phase_hint = transport.phase_hint();
        let connector_timeouts = PhasedTimeouts::from_resolved(&timeouts);
        let send_fut = async move {
            let res = transport
                .send_request(request, connector_timeouts, proxy_url)
                .await;
            if res.is_ok() {
                record_pool_completion(&pool, host_key, &host);
            }
            res
        };

        let response = race_dispatch(send_fut, &deadlines, phase_hint, cancel_for_send).await?;

        Ok(wrap_upstream_response(
            response,
            cancel,
            timeouts.body_chunk_ms,
            deadlines.total_deadline,
            is_streaming,
        ))
    }
}

#[cfg(feature = "upstream-hyper")]
fn build_host_key_and_uri(url: &str) -> UpstreamResult<(Uri, HostKey, String)> {
    let uri: Uri = url
        .parse()
        .map_err(|e: http::uri::InvalidUri| UpstreamError::Invalid(e.to_string()))?;
    let scheme = Scheme::from_uri(uri.scheme_str().unwrap_or("http"));
    let host = uri.host().unwrap_or("").to_string();
    let port = uri
        .port_u16()
        .unwrap_or(if matches!(scheme, Scheme::Https) {
            443
        } else {
            80
        });
    let host_key = HostKey::new(scheme, &host, port);
    Ok((uri, host_key, host))
}

#[cfg(feature = "upstream-hyper")]
fn build_hyper_request(spec: UpstreamRequest) -> UpstreamResult<Request<Full<Bytes>>> {
    let body_bytes = spec.body;
    let body_len = body_bytes.as_ref().map(bytes::Bytes::len);
    let body: Full<Bytes> = match body_bytes {
        Some(bytes) => Full::new(bytes),
        None => Full::new(Bytes::new()),
    };
    let mut builder = Request::builder().method(spec.method).uri(&spec.url);
    {
        let headers = builder
            .headers_mut()
            .ok_or_else(|| UpstreamError::Invalid("failed to build request headers".to_string()))?;
        *headers = spec.headers;
        if let Some(len) = body_len
            && !headers.contains_key(http::header::CONTENT_LENGTH)
            && let Ok(v) = http::HeaderValue::from_str(&len.to_string())
        {
            headers.insert(http::header::CONTENT_LENGTH, v);
        }
    }
    builder
        .body(body)
        .map_err(|e| UpstreamError::Invalid(e.to_string()))
}

#[cfg(feature = "upstream-hyper")]
fn record_pool_completion(pool: &Pool, host_key: HostKey, host: &str) {
    if pool.total() == 0 {
        pool.record_dial(host_key);
    } else {
        pool.record_reuse(host_key);
    }
    tracing::debug!(host = %host, "upstream request completed");
}

#[cfg(feature = "upstream-hyper")]
fn handle_dispatch_error(e: UpstreamError) -> UpstreamError {
    if let Some(phase) = recover_phased_phase(&e) {
        UpstreamError::Timeout(phase)
    } else {
        e
    }
}

#[cfg(feature = "upstream-hyper")]
fn wrap_upstream_response(
    response: hyper::Response<hyper::body::Incoming>,
    cancel: CancellationToken,
    body_chunk_ms: u64,
    total_deadline: Instant,
    is_streaming: bool,
) -> UpstreamResponse {
    let (parts, body) = response.into_parts();
    let body_stream = UpstreamBodyStream::from_hyper(
        body,
        cancel,
        body_chunk_ms,
        total_deadline,
        8 * 1024 * 1024,
        is_streaming,
    );
    UpstreamResponse {
        status: parts.status,
        headers: parts.headers,
        body: body_stream,
    }
}

#[cfg(feature = "upstream-hyper")]
async fn race_earliest_timeout(
    deadlines: &ResolvedPhaseDeadlines,
    phase_hint: Option<UpstreamPhase>,
) -> UpstreamError {
    let phase_hint_sleep = async {
        match phase_hint {
            Some(phase) => {
                tokio::time::sleep_until(tokio::time::Instant::from_std(
                    deadlines.deadline_for(phase),
                ))
                .await;
                phase
            }
            None => std::future::pending::<UpstreamPhase>().await,
        }
    };
    let total_sleep = async {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadlines.total_deadline)).await;
        UpstreamPhase::Total
    };
    let write_sleep = async {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadlines.write_deadline)).await;
        UpstreamPhase::Write
    };
    let headers_sleep = async {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadlines.headers_deadline)).await;
        UpstreamPhase::Headers
    };

    tokio::pin!(total_sleep);
    tokio::pin!(write_sleep);
    tokio::pin!(headers_sleep);
    tokio::pin!(phase_hint_sleep);

    let phase = tokio::select! {
        biased;
        p = &mut total_sleep => p,
        p = &mut phase_hint_sleep, if phase_hint.is_some() => p,
        p = &mut write_sleep => p,
        p = &mut headers_sleep => p,
    };
    UpstreamError::Timeout(phase)
}

#[cfg(feature = "upstream-hyper")]
async fn race_dispatch<F>(
    send_fut: F,
    deadlines: &ResolvedPhaseDeadlines,
    phase_hint: Option<UpstreamPhase>,
    cancel: CancellationToken,
) -> UpstreamResult<hyper::Response<hyper::body::Incoming>>
where
    F: std::future::Future<Output = UpstreamResult<hyper::Response<hyper::body::Incoming>>>,
{
    let timeout_fut = race_earliest_timeout(deadlines, phase_hint);
    tokio::pin!(send_fut);
    tokio::pin!(timeout_fut);

    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(UpstreamError::Cancel),
        timeout_err = &mut timeout_fut => Err(timeout_err),
        res = &mut send_fut => res.map_err(handle_dispatch_error),
    }
}

#[cfg(feature = "upstream-hyper")]
fn spawn_eviction_loop(pool: Pool) {
    // The eviction loop is a non-critical best-effort cleanup task.
    // Skip it when no Tokio runtime is active — this is the case for
    // `#[test]` (sync) call sites that build an `UpstreamClient` and
    // drop it without running a Tokio reactor.
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            let evicted = pool.evict_older_than(std::time::Duration::from_mins(1));
            if evicted > 0 {
                tracing::debug!(evicted, "upstream pool eviction sweep");
            }
        }
    });
}

#[cfg(feature = "upstream-hyper")]
fn push_unique_source_str(parts: &mut Vec<String>, s: String) {
    if !s.is_empty() && !parts.contains(&s) {
        parts.push(s);
    }
}

/// Format a hyper-util legacy `Error` with its full `source()` chain
/// so the operator can see the root cause (e.g. "connection closed
/// before message completed", "broken pipe", "tls handshake eof").
/// The default `Display` only gives "client error (SendRequest)"
/// which is useless for debugging.
#[cfg(feature = "upstream-hyper")]
fn format_hyper_error(e: &hyper_util::client::legacy::Error) -> String {
    use std::error::Error as _;
    let mut parts: Vec<String> = vec![e.to_string()];
    let mut current: Option<&(dyn std::error::Error + 'static)> = e.source();
    while let Some(c) = current {
        push_unique_source_str(&mut parts, c.to_string());
        current = c.source();
    }
    parts.join(": ")
}

#[cfg(feature = "upstream-hyper")]
fn map_phased_connector_error(p: &super::connector::PhasedConnectorError) -> UpstreamError {
    match &p.kind {
        super::connector::PhasedErrorKind::Timeout => UpstreamError::Timeout(p.phase),
        super::connector::PhasedErrorKind::InvalidUri(s) => {
            UpstreamError::Invalid(format!("in phase `{}`: {}", p.phase, s))
        }
        super::connector::PhasedErrorKind::Io(io_err) => {
            if p.phase == super::UpstreamPhase::Tls {
                UpstreamError::Tls(format!("in phase `{}`: {}", p.phase, io_err))
            } else {
                UpstreamError::Connection(format!("in phase `{}`: {}", p.phase, io_err))
            }
        }
    }
}

/// Walk the `source()` chain of a hyper `Error` looking for a
/// `PhasedConnectorError` and map it to an `UpstreamError`.
#[cfg(feature = "upstream-hyper")]
fn hyper_source_connector_error(e: &hyper_util::client::legacy::Error) -> Option<UpstreamError> {
    use std::error::Error as _;
    let mut current: Option<&(dyn std::error::Error + 'static)> = e.source();
    while let Some(c) = current {
        if let Some(p) = c.downcast_ref::<super::connector::PhasedConnectorError>() {
            return Some(map_phased_connector_error(p));
        }
        current = c.source();
    }
    None
}

/// Walk the `source()` chain of a hyper `Error` looking for a
/// `PhasedConnectorError` and return its phase. Returns `None` if
/// the chain does not contain one (e.g. a non-phased test connector).
#[cfg(feature = "upstream-hyper")]
fn hyper_source_phase(e: &hyper_util::client::legacy::Error) -> Option<UpstreamPhase> {
    use std::error::Error as _;
    let mut current: Option<&(dyn std::error::Error + 'static)> = e.source();
    while let Some(c) = current {
        if let Some(p) = phased_phase(c) {
            return Some(p);
        }
        current = c.source();
    }
    None
}

/// Walk the `source()` chain of an `UpstreamError` looking for a
/// `PhasedConnectorError` and return its phase. This is the fallback
/// used in `call_inner` for any error variant that exposes a source
/// (currently only `UpstreamError::Connection`). In the normal flow,
/// `ProductionDispatch` and `TestDispatch` convert the phased
/// connector error to `Timeout(phase)` directly, so this is a
/// belt-and-suspenders for any path that doesn't.
#[cfg(feature = "upstream-hyper")]
fn recover_phased_phase(e: &UpstreamError) -> Option<UpstreamPhase> {
    use std::error::Error as _;
    let mut current: Option<&(dyn std::error::Error + 'static)> = e.source();
    while let Some(c) = current {
        if let Some(p) = phased_phase(c) {
            return Some(p);
        }
        current = c.source();
    }
    None
}

#[cfg(feature = "upstream-hyper")]
#[derive(Clone)]
struct TaskLocalExecutor;

#[cfg(feature = "upstream-hyper")]
impl<F> hyper::rt::Executor<F> for TaskLocalExecutor
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, future: F) {
        let timeouts = CALL_TIMEOUTS.try_with(|t| *t).ok();
        let proxy = CALL_PROXY.try_with(std::clone::Clone::clone).ok().flatten();

        let fut = async move {
            match (timeouts, proxy) {
                (Some(t), Some(p)) => {
                    CALL_TIMEOUTS
                        .scope(t, CALL_PROXY.scope(Some(p), future))
                        .await
                }
                (Some(t), None) => CALL_TIMEOUTS.scope(t, future).await,
                (None, Some(p)) => CALL_PROXY.scope(Some(p), future).await,
                (None, None) => future.await,
            }
        };
        tokio::spawn(fut);
    }
}
