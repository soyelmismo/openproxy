//! The `UpstreamClient`: a hyper-based HTTP client with per-phase timeouts.
//!
//! See the module-level docs in `mod.rs` for the architecture; this file is
//! the implementation.

use std::sync::Arc;
use std::time::Instant;

use super::request_body::RequestBody;
use super::upload::{FlushProgress, ObservedConnector};
use bytes::Bytes;
use http::{HeaderMap, Method, Request, Uri};

use super::cancel::CancellationToken;
use super::conn_pool::{HostKey, Scheme, UpstreamConnectionPool as Pool};
use super::error::{UpstreamError, UpstreamResult};
use super::phases::{ResolvedPhaseDeadlines, UpstreamPhase};
use super::profile::TimeoutProfile;
use super::response::{UpstreamBodyStream, UpstreamResponse};

#[cfg(feature = "upstream-hyper")]
use super::connector::{CALL_PROXY, CALL_TIMEOUTS, PhasedConnector, PhasedTimeouts, phased_phase};
#[cfg(feature = "upstream-hyper")]
use dashmap::DashMap;
#[cfg(feature = "upstream-hyper")]
use hyper_util::client::legacy::Client as HyperClient;
#[cfg(feature = "upstream-hyper")]
use hyper_util::client::legacy::connect::Connection as HyperConnection;
#[cfg(feature = "upstream-hyper")]
use hyper_util::rt::TokioExecutor;

// UpstreamRequest

/// Caller-supplied request shape: URL, method, headers and a bounded body.
#[derive(Debug, Clone)]
pub struct UpstreamRequest {
    pub method: Method,
    pub url: String,
    pub headers: HeaderMap,
    pub body: Option<Bytes>,
    /// `false` disables the `idle_chunk_ms` body-chunk gap timeout, leaving
    /// only `total_ms` on the body read. Set it for non-streaming requests,
    /// where the upstream generates the full reply before sending anything.
    /// Default: `true`.
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
        let mut headers = HeaderMap::with_capacity(1);
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

    /// A POST with a caller-built multipart body. `content_type` must carry
    /// the boundary, e.g.
    /// `multipart/form-data; boundary=----WebKitFormBoundary...`. The audio
    /// transcription handler uses this to forward a pre-built body to
    /// `/audio/transcriptions`.
    pub fn post_multipart(url: impl Into<String>, content_type: &str, body: Bytes) -> Self {
        let mut headers = HeaderMap::with_capacity(1);
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
            // server-side, so the body-chunk gap timeout would kill it.
            is_streaming: false,
            proxy: None,
            proxy_status: None,
        }
    }
}

// UpstreamClient

pub trait UpstreamTransport: Send + Sync + std::fmt::Debug {
    fn send_request(
        &self,
        req: Request<RequestBody>,
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
/// `Pool` is only the observability layer over the hyper client's own
/// internal pool.
pub struct UpstreamClient {
    pool: Pool,
    #[cfg(feature = "upstream-hyper")]
    transport: Arc<dyn UpstreamTransport>,
}

#[derive(Debug)]
pub(crate) struct ProductionTransport {
    pub(crate) direct_client: HyperClient<ObservedConnector<PhasedConnector>, RequestBody>,
    pub(crate) proxy_clients:
        Arc<DashMap<String, HyperClient<ObservedConnector<PhasedConnector>, RequestBody>>>,
}

impl ProductionTransport {
    pub(crate) fn new() -> Self {
        let connector = PhasedConnector::with_defaults();
        let direct_client = HyperClient::builder(TaskLocalExecutor)
            .pool_max_idle_per_host(8)
            .pool_idle_timeout(std::time::Duration::from_secs(20))
            .build(ObservedConnector(connector));
        Self {
            direct_client,
            proxy_clients: Arc::new(DashMap::new()),
        }
    }

    pub(crate) fn client_for_proxy(
        &self,
        proxy_url: Option<&str>,
    ) -> HyperClient<ObservedConnector<PhasedConnector>, RequestBody> {
        match proxy_url {
            None => self.direct_client.clone(),
            Some(proxy) => {
                if let Some(client) = self.proxy_clients.get(proxy) {
                    client.clone()
                } else {
                    let connector = PhasedConnector::with_defaults();
                    let client = HyperClient::builder(TaskLocalExecutor)
                        .pool_max_idle_per_host(8)
                        .pool_idle_timeout(std::time::Duration::from_secs(20))
                        .build(ObservedConnector(connector));
                    if self.proxy_clients.len() >= 128 {
                        self.proxy_clients.clear();
                    }
                    self.proxy_clients.insert(proxy.to_string(), client.clone());
                    client
                }
            }
        }
    }
}

impl UpstreamTransport for ProductionTransport {
    fn send_request(
        &self,
        req: Request<RequestBody>,
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
        let client = self.client_for_proxy(proxy_url.as_deref());
        Box::pin(async move {
            let fut = async move {
                client.request(req).await.map_err(|e| {
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
    hyper: HyperClient<ObservedConnector<C>, RequestBody>,
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
        req: Request<RequestBody>,
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
                        UpstreamError::Http(format_hyper_error(&e))
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
    /// A client with the default connector (HTTPS via rustls, HTTP plain).
    pub fn new() -> Arc<Self> {
        #[cfg(feature = "upstream-hyper")]
        {
            let pool = Pool::new();
            pool.spawn_eviction_loop();
            Arc::new(Self {
                pool,
                transport: Arc::new(ProductionTransport::new()),
            })
        }
        #[cfg(not(feature = "upstream-hyper"))]
        {
            let pool = Pool::new();
            pool.spawn_eviction_loop();
            Arc::new(Self { pool })
        }
    }

    /// Test-only client with a custom connector (`Clone` +
    /// `tower_service::Service<Uri>` over the hyper-util connect future type).
    /// `phase_hint` is reported when a timeout fires during the
    /// connect/headers phase; without one the error falls back to
    /// `Timeout(Headers)`.
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
        let hyper: HyperClient<ObservedConnector<C>, RequestBody> =
            HyperClient::builder(TokioExecutor::new())
                .pool_max_idle_per_host(0)
                .build(ObservedConnector(connector));
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
        let (_uri, host_key, host) = build_host_key_and_uri(&spec.url, proxy_url.clone())?;
        let request = build_hyper_request(spec)?;
        let upload_progress = request
            .extensions()
            .get::<tokio::sync::watch::Receiver<Option<Instant>>>()
            .ok_or_else(|| UpstreamError::Invalid("missing upload observer".into()))?
            .clone();
        let mut request = request;
        let captured = hyper_util::client::legacy::connect::capture_connection(&mut request);

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

        let (response, ttft_deadline) = race_dispatch(
            send_fut,
            &deadlines,
            phase_hint,
            cancel_for_send,
            (captured, upload_progress),
            is_streaming,
            &timeouts,
        )
        .await?;

        wrap_upstream_response(
            response,
            cancel,
            timeouts.body_chunk_ms,
            ttft_deadline,
            deadlines.total_deadline,
            is_streaming,
        )
    }
}

#[cfg(feature = "upstream-hyper")]
fn build_host_key_and_uri(
    url: &str,
    proxy: Option<String>,
) -> UpstreamResult<(Uri, HostKey, String)> {
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
    let host_key = HostKey::with_proxy(scheme, &host, port, proxy);
    Ok((uri, host_key, host))
}

#[cfg(feature = "upstream-hyper")]
static DEFAULT_USER_AGENT: std::sync::LazyLock<http::HeaderValue> =
    std::sync::LazyLock::new(|| {
        if let Ok(custom) = std::env::var("OPENPROXY_USER_AGENT")
            && let Ok(val) = http::HeaderValue::from_str(&custom)
        {
            val
        } else {
            http::HeaderValue::from_static(concat!("openproxy/", env!("CARGO_PKG_VERSION")))
        }
    });

#[cfg(feature = "upstream-hyper")]
pub(crate) fn build_hyper_request(spec: UpstreamRequest) -> UpstreamResult<Request<RequestBody>> {
    let body_bytes = spec.body;
    let body_len = body_bytes.as_ref().map(bytes::Bytes::len);
    let (body, upload_progress) = RequestBody::new(body_bytes.unwrap_or_default());
    let mut builder = Request::builder().method(spec.method).uri(&spec.url);
    {
        let headers = builder
            .headers_mut()
            .ok_or_else(|| UpstreamError::Invalid("failed to build request headers".to_string()))?;
        *headers = spec.headers;
        super::compression::negotiate_request(headers, spec.is_streaming);
        if !headers.contains_key(http::header::USER_AGENT) {
            headers.insert(http::header::USER_AGENT, DEFAULT_USER_AGENT.clone());
        }
        if let Some(len) = body_len
            && !headers.contains_key(http::header::CONTENT_LENGTH)
        {
            let mut buf = itoa::Buffer::new();
            let len_str = buf.format(len);
            let mut bytes = bytes::BytesMut::with_capacity(len_str.len());
            bytes.extend_from_slice(len_str.as_bytes());
            if let Ok(v) = http::HeaderValue::from_maybe_shared(bytes.freeze()) {
                headers.insert(http::header::CONTENT_LENGTH, v);
            }
        }
    }
    let mut request = builder
        .body(body)
        .map_err(|e| UpstreamError::Invalid(e.to_string()))?;
    request.extensions_mut().insert(upload_progress);
    Ok(request)
}

#[cfg(feature = "upstream-hyper")]
fn record_pool_completion(pool: &Pool, host_key: HostKey, host: &str) {
    if !pool.contains_host(&host_key) {
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

/// Wrap the hyper response, installing the content-decoding the response
/// headers call for.
///
/// Fails before any body byte is read when the upstream applied a codec this
/// transport cannot invert, so a parser never sees compressed bytes presented
/// as plaintext.
#[cfg(feature = "upstream-hyper")]
fn wrap_upstream_response(
    response: hyper::Response<hyper::body::Incoming>,
    cancel: CancellationToken,
    body_chunk_ms: u64,
    ttft_deadline: Instant,
    total_deadline: Instant,
    is_streaming: bool,
) -> UpstreamResult<UpstreamResponse> {
    let (mut parts, body) = response.into_parts();
    let decoding = super::compression::BodyDecoding::from_response(&parts.headers)?;
    decoding.strip_decoded_headers(&mut parts.headers);
    let limit = if is_streaming {
        super::response::STREAMING_BODY_LIMIT_BYTES
    } else {
        super::response::NON_STREAMING_BODY_LIMIT_BYTES
    };
    let body_stream = UpstreamBodyStream::from_hyper(
        body,
        decoding,
        cancel,
        body_chunk_ms,
        (ttft_deadline, total_deadline),
        limit,
        is_streaming,
    );
    Ok(UpstreamResponse {
        status: parts.status,
        headers: parts.headers,
        body: body_stream,
    })
}

#[cfg(feature = "upstream-hyper")]
async fn race_dispatch<F>(
    send_fut: F,
    deadlines: &ResolvedPhaseDeadlines,
    phase_hint: Option<UpstreamPhase>,
    cancel: CancellationToken,
    progress: (
        hyper_util::client::legacy::connect::CaptureConnection,
        tokio::sync::watch::Receiver<Option<Instant>>,
    ),
    is_streaming: bool,
    timeouts: &super::profile::ResolvedTimeouts,
) -> UpstreamResult<(hyper::Response<hyper::body::Incoming>, Instant)>
where
    F: std::future::Future<Output = UpstreamResult<hyper::Response<hyper::body::Incoming>>>,
{
    let (mut captured, mut upload) = progress;
    let upload_status = upload.clone();
    let write_ms = timeouts.write_ms;
    let headers_ms = timeouts.headers_ms;
    // New and pooled connections share the same per-request capture boundary.
    // DNS/dial/TLS have their own timers; upload starts only after connecting.
    let upload_done = async {
        let mut flushed = {
            let metadata = captured.wait_for_connection_metadata().await;
            let mut extras = http::Extensions::new();
            metadata
                .as_ref()
                .ok_or_else(|| UpstreamError::Http("missing connection metadata".into()))?
                .get_extras(&mut extras);
            extras
                .get::<FlushProgress>()
                .ok_or_else(|| UpstreamError::Http("missing flush observer".into()))?
                .0
                .clone()
        };
        let wait = async {
            let consumed_at = loop {
                if let Some(at) = *upload.borrow_and_update() {
                    break at;
                }
                upload
                    .changed()
                    .await
                    .map_err(|_| UpstreamError::Http("upload interrupted".into()))?;
            };
            loop {
                if let Some(at) = *flushed.borrow_and_update()
                    && at >= consumed_at
                {
                    return Ok::<_, UpstreamError>(at);
                }
                flushed
                    .changed()
                    .await
                    .map_err(|_| UpstreamError::Http("connection closed during upload".into()))?;
            }
        };
        tokio::time::timeout(std::time::Duration::from_millis(write_ms), wait)
            .await
            .map_err(|_| UpstreamError::Timeout(UpstreamPhase::Write))?
    };
    tokio::pin!(send_fut);
    tokio::pin!(upload_done);
    let total_sleep = tokio::time::sleep_until(deadlines.total_deadline.into());
    tokio::pin!(total_sleep);
    let phase_hint_sleep = async {
        if let Some(phase) = phase_hint {
            tokio::time::sleep_until(deadlines.deadline_for(phase).into()).await;
            UpstreamError::Timeout(phase)
        } else {
            std::future::pending().await
        }
    };
    tokio::pin!(phase_hint_sleep);
    let uploaded_at = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(UpstreamError::Cancel),
        () = &mut total_sleep => return Err(UpstreamError::Timeout(UpstreamPhase::Total)),
        err = &mut phase_hint_sleep => return Err(err),
        res = &mut send_fut => {
            let response = res.map_err(handle_dispatch_error)?;
            // An early response (e.g. a 4xx while uploading) must be returned,
            // not replaced by an upload timeout. Do not restart TTFT at headers.
            let uploaded = (*upload_status.borrow()).unwrap_or_else(Instant::now);
            let ttft = if is_streaming {
                uploaded + std::time::Duration::from_millis(headers_ms)
            } else { deadlines.total_deadline };
            return Ok((response, ttft));
        },
        at = &mut upload_done => at?,
    };
    let ttft_deadline = if is_streaming {
        uploaded_at + std::time::Duration::from_millis(headers_ms)
    } else {
        deadlines.total_deadline
    };
    let headers_sleep = tokio::time::sleep_until(ttft_deadline.into());
    tokio::pin!(headers_sleep);
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(UpstreamError::Cancel),
        () = &mut total_sleep => Err(UpstreamError::Timeout(UpstreamPhase::Total)),
        () = &mut headers_sleep, if is_streaming => Err(UpstreamError::Timeout(UpstreamPhase::Headers)),
        res = &mut send_fut => res.map(|r| (r, ttft_deadline)).map_err(handle_dispatch_error),
    }
}

#[cfg(feature = "upstream-hyper")]
fn push_unique_source_str(parts: &mut Vec<String>, s: String) {
    if !s.is_empty() && !parts.contains(&s) {
        parts.push(s);
    }
}

/// Format a hyper-util legacy `Error` with its full `source()` chain, so the
/// root cause surfaces ("connection closed before message completed", "broken
/// pipe", "tls handshake eof"). The default `Display` is only
/// "client error (SendRequest)".
#[cfg(feature = "upstream-hyper")]
fn format_hyper_error(e: &hyper_util::client::legacy::Error) -> String {
    use std::error::Error as _;
    let mut parts: Vec<String> = Vec::with_capacity(3);
    parts.push(e.to_string());
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

/// Map the first `PhasedConnectorError` in a hyper `Error` source chain to an
/// `UpstreamError`.
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

/// Walk a hyper `Error` source chain for a `PhasedConnectorError` and map it
/// to an `UpstreamError`. `None` when the chain holds none (e.g. a non-phased
/// test connector).
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

/// Recover the phase from an `UpstreamError` source chain. `call_inner` uses
/// it for any variant exposing a source (currently only
/// `UpstreamError::Connection`); the normal flow converts the phased error in
/// `ProductionDispatch` / `TestDispatch`.
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
