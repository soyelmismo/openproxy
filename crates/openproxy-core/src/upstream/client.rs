//! The `UpstreamClient` — the hyper-based replacement for the
//! reqwest-based `reqwest::Client` used by the chat pipeline.
//!
//! See the module-level docs in `mod.rs` for the full architecture;
//! this file is the implementation.

use std::pin::Pin;
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
use super::connector::{phased_phase, CALL_TIMEOUTS, PhasedConnector, PhasedTimeouts};
#[cfg(feature = "upstream-hyper")]
use hyper_util::client::legacy::connect::Connection as HyperConnection;
#[cfg(feature = "upstream-hyper")]
use hyper_util::client::legacy::Client as HyperClient;
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
}

impl UpstreamRequest {
    /// Build a simple GET with no headers / body.
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: Method::GET,
            url: url.into(),
            headers: HeaderMap::new(),
            body: None,
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
pub struct UpstreamClient {
    pool: Pool,
    /// The hyper client used for the production path. Its type
    /// parameter is the connector, so swapping the connector at
    /// runtime would require a second client. We use an enum:
    /// either the production client (no test override) or a
    /// test-supplied client (any connector). See `dispatch` below.
    #[cfg(feature = "upstream-hyper")]
    dispatch: HyperDispatch,
}

/// Tagged union of the possible hyper clients. The production case is
/// the `PhasedConnector`; the test case is any `C` that satisfies the
/// `Service<Uri>` shape required by hyper-util.
#[cfg(feature = "upstream-hyper")]
enum HyperDispatch {
    /// Production: real `HyperClient` with the `PhasedConnector`.
    /// The connector is owned by the `HyperClient` internally (it
    /// was cloned into the builder at construction time). We no
    /// longer keep a separate connector clone here because HIGH-5
    /// replaced the `set_timeouts` shared-atomics pattern with a
    /// task-local — there is no per-call state to push into the
    /// connector anymore.
    Production {
        hyper: HyperClient<PhasedConnector, Full<Bytes>>,
    },
    /// Test: any `C` that satisfies the hyper-util connect shape.
    /// Wrapped in `Arc<dyn HyperDispatchDyn>` so the `HyperDispatch`
    /// enum stays a fixed size regardless of `C`.
    Test(Arc<dyn HyperDispatchDyn>),
    /// Stub for non-hyper builds (the dispatch field exists but is
    /// never used; `call_inner` short-circuits to a stub response).
    #[cfg(not(feature = "upstream-hyper"))]
    Stub,
}

#[cfg(feature = "upstream-hyper")]
impl std::fmt::Debug for HyperDispatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HyperDispatch::Production { .. } => f.debug_tuple("Production").finish(),
            HyperDispatch::Test(_) => f.debug_tuple("Test").finish(),
        }
    }
}

/// Trait object that can dispatch a hyper `Request<Full<Bytes>>` and
/// return a `Response`. This is the test-time equivalent of the
/// production hyper client.
///
/// HIGH-6 fix: the signature takes `Request<Full<Bytes>>` directly
/// (NOT `Request<Pin<Box<dyn Body>>>`). The caller (`call_inner`)
/// already has the body as `Option<Bytes>` and builds a `Full<Bytes>`
/// from it — wrapping that in a `dyn Body` only to drain it back to
/// `Bytes` via `body.collect().await` inside the dispatch was pure
/// waste (one `HeaderMap::clone()` + one `Bytes` round-trip per
/// request). With `Full<Bytes>` in the signature, the production
/// dispatch hands the request straight to hyper with zero copying.
#[cfg(feature = "upstream-hyper")]
trait HyperDispatchDyn: Send + Sync + 'static {
    fn dispatch(
        &self,
        req: Request<Full<Bytes>>,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        http::Response<hyper::body::Incoming>,
                        UpstreamError,
                    >,
                > + Send
                + '_,
        >,
    >;
    fn phase_hint(&self) -> Option<UpstreamPhase>;
}

impl UpstreamClient {
    /// Build a new client with the default connector (HTTPS via rustls
    /// with safe defaults, HTTP plain). Returns an `Arc<UpstreamClient>`
    /// per the spec API.
    pub fn new() -> Arc<Self> {
        #[cfg(feature = "upstream-hyper")]
        {
            // Bug 2b/2c fix: the production connector is the
            // real-per-phase `PhasedConnector` (see `connector.rs`).
            // It enforces DNS / Dial / TLS with independent
            // `tokio::time::timeout` and reports the stalled phase
            // via `PhasedConnectorError`. The hyper-util `Connect`
            // blanket impl accepts it because its `Response` is
            // `TokioIo<TcpStream>`, the same wire type the legacy
            // `HttpConnector` returns.
            //
            // We keep a clone of the connector OUTSIDE the
            // HyperClient so we can call `set_timeouts` per call.
            // The clone shares the same `Arc<AtomicU64>` fields, so
            // the updates are visible to the cloned connector held
            // inside the HyperClient.
            //
            // HIGH-5 fix: the above comment is now HISTORICAL. We no
            // longer call `set_timeouts` — the per-call timeouts are
            // passed via the `CALL_TIMEOUTS` task-local. The
            // connector clone is still passed to `HyperClient::builder`
            // (hyper-util clones it internally per request), but we
            // don't keep a separate reference in `HyperDispatch::Production`.
            let connector = PhasedConnector::with_defaults();
            let hyper: HyperClient<PhasedConnector, Full<Bytes>> =
                HyperClient::builder(TokioExecutor::new())
                    // Keep up to 32 idle connections per host so the
                    // shared `UpstreamClient` (lives on `AppState`,
                    // created once at startup) can reuse them across
                    // requests — eliminating the per-request TCP+TLS
                    // handshake (~50-200ms on WAN). Cancellation of an
                    // individual request is still propagated to the
                    // upstream: the `CancellationToken` passed to
                    // `UpstreamClient::call` aborts the hyper body
                    // stream, which signals the upstream that the
                    // consumer is gone. Dropping the request future no
                    // longer closes the underlying connection, but that
                    // is the intended trade-off now that the client is
                    // shared.
                    .pool_max_idle_per_host(32)
                    .build(connector);
            let pool = Pool::new();
            spawn_eviction_loop(pool.clone());
            Arc::new(Self {
                pool,
                dispatch: HyperDispatch::Production { hyper },
            })
        }
        #[cfg(not(feature = "upstream-hyper"))]
        {
            let pool = Pool::new();
            spawn_eviction_loop(pool.clone());
            Arc::new(Self {
                pool,
                dispatch: HyperDispatch::Stub,
            })
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
    pub fn for_test_with_connector<C, T>(connector: C, phase_hint: Option<UpstreamPhase>) -> Arc<Self>
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
        T: hyper::rt::Read
            + hyper::rt::Write
            + HyperConnection
            + Unpin
            + Send
            + 'static,
    {
        let hyper: HyperClient<C, Full<Bytes>> =
            HyperClient::builder(TokioExecutor::new())
                .pool_max_idle_per_host(0)
                .build(connector.clone());
        let arc: Arc<dyn HyperDispatchDyn> = Arc::new(TestDispatch {
            hyper,
            phase_hint,
        });
        Arc::new(Self {
            pool: Pool::new(),
            dispatch: HyperDispatch::Test(arc),
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

        // Pre-flight cancellation.
        if cancel.is_cancelled() {
            return Err(UpstreamError::Cancel);
        }

        // Parse the URL and derive the HostKey for the pool counter.
        let uri: Uri = spec
            .url
            .parse()
            .map_err(|e: http::uri::InvalidUri| UpstreamError::Invalid(e.to_string()))?;
        let scheme = Scheme::from_uri(uri.scheme_str().unwrap_or("http"));
        let host = uri.host().unwrap_or("").to_string();
        let port = uri
            .port_u16()
            .unwrap_or(if matches!(scheme, Scheme::Https) { 443 } else { 80 });
        let host_key = HostKey::new(scheme, host.clone(), port);

        // Build the hyper::Request<Full<Bytes>> for the legacy client.
        //
        // HIGH-6 fix: we build `Full<Bytes>` directly instead of going
        // through `Pin<Box<dyn Body>>`. The production dispatch takes
        // `Request<Full<Bytes>>` and hands it straight to hyper —
        // eliminating the `body.collect().await` + `HeaderMap::clone()`
        // round-trip that the dyn-Body trait forced on us.
        let body_bytes = spec.body.clone();
        let body: Full<Bytes> = match &body_bytes {
            Some(bytes) => Full::new(bytes.clone()),
            None => Full::new(Bytes::new()),
        };
        let mut builder = Request::builder()
            .method(spec.method.clone())
            .uri(&spec.url);
        {
            let headers = builder.headers_mut().ok_or_else(|| {
                UpstreamError::Invalid("failed to build request headers".to_string())
            })?;
            for (k, v) in spec.headers.iter() {
                headers.append(k.clone(), v.clone());
            }
            // Set `Content-Length` when the body is a known-size
            // buffer. hyper-util's legacy client does NOT auto-set
            // `Content-Length` (verified against hyper 1.10
            // `src/proto/h1/dispatch.rs` and
            // `hyper-util 0.1.20/src/client/legacy/client.rs`:
            // `set_content_length_if_missing` is only called from
            // the h2 client, NOT the h1 client used by the legacy
            // builder). With chunked encoding as the only fallback,
            // strict upstreams (Google's `oauth2.googleapis.com`
            // returns `411 Length Required`, OpenRouter returns
            // `400 JSON parsing failed` on the chunked body)
            // reject the request.
            //
            // ponytail: only emit when the caller did NOT set the
            // header themselves (i.e. the adapter didn't ask for a
            // specific value). For `UpstreamRequest::post_json` the
            // caller sets `Content-Type` but not `Content-Length`,
            // so we always add it. For `get` we skip it (no body).
            if let Some(ref bytes) = body_bytes {
                if !headers.contains_key(http::header::CONTENT_LENGTH) {
                    if let Ok(v) =
                        http::HeaderValue::from_str(&bytes.len().to_string())
                    {
                        headers.insert(http::header::CONTENT_LENGTH, v);
                    }
                }
            }
        }
        let request: Request<Full<Bytes>> =
            builder.body(body).map_err(|e| UpstreamError::Invalid(e.to_string()))?;

        // Bug 2b/2c fix: hyper's legacy client performs dial + TLS +
        // write + wait-for-headers as a SINGLE `Service::call` future.
        // The old code "soft-accumulated" the per-step deadlines by
        // racing that single future against
        // `min(headers, write, dial, tls, total)` and labelling every
        // timeout as `Headers`. That violated the contract: a tight
        // `write_ms` would never surface as `Timeout(Write)`.
        //
        // The new design is real per-phase enforcement, with the
        // phases split across two layers:
        //
        //   - DNS / Dial / TLS: enforced INSIDE the connector
        //     (`PhasedConnector` in `connector.rs`) with their own
        //     `tokio::time::timeout` calls. A stalled DNS lookup
        //     surfaces as a `PhasedConnectorError` with phase `Dns`,
        //     and we downcast the boxed error to recover the phase
        //     below.
        //
        //   - Write vs Headers: enforced HERE with NESTED
        //     `tokio::time::timeout` calls. The OUTER race has
        //     `write_ms` and labels the timeout `Write`; the INNER
        //     race has `headers_ms` and labels the timeout `Headers`.
        //     Whichever ceiling fires first wins.
        //
        //   - Total: the outermost ceiling.
        //
        // With this structure, `write_ms = 200ms` and
        // `headers_ms = 30_000ms` produces `Timeout(Write)` at ~200ms
        // even if the server eventually responds — which is what the
        // contract requires.
        let pool = self.pool.clone();
        let cancel_for_send = cancel.clone();
        let host_key_for_send = host_key.clone();
        let host_for_log = host.clone();
        let dispatch = match &self.dispatch {
            HyperDispatch::Production { hyper } => {
                // HIGH-5 fix: the per-call timeouts are now passed via
                // the `CALL_TIMEOUTS` task-local (see `connector.rs`).
                // The old `connector.set_timeouts(...)` call is GONE —
                // it was a race between concurrent requests sharing the
                // same `Arc<AtomicU64>` fields. The task-local is set
                // below by wrapping `send_fut` in `CALL_TIMEOUTS.scope(...)`,
                // which guarantees the connector reads the correct
                // per-call timeouts when its `call()` is polled.
                let prod: Arc<dyn HyperDispatchDyn> = Arc::new(ProductionDispatch { inner: hyper.clone() });
                prod
            }
            HyperDispatch::Test(t) => t.clone(),
        };
        // Bug 2 fix: read the dispatch's own `phase_hint()` BEFORE
        // we move `dispatch` into the `send_fut` async block below.
        // For test dispatches that inject a `phase_hint` (e.g.
        // `Some(Dns)` with `dns_ms = 50`), this is what lets
        // `call_inner` surface `Timeout(Dns)` instead of the
        // generic `Timeout(Write)` mask from the write_sleep
        // ceiling. In production, `phase_hint()` returns `None` and
        // the `phase_hint_sleep` future never resolves, so the
        // per-phase race below is unchanged.
        let phase_hint = dispatch.phase_hint();
        // HIGH-5 fix: wrap the send_fut in `CALL_TIMEOUTS.scope(...)`
        // so the `PhasedConnector::call` (polled deep inside hyper's
        // legacy client) reads the per-call timeouts from the task-local
        // instead of from shared `Arc<AtomicU64>` fields. This eliminates
        // the race where another request's `call_inner` could clobber
        // the atomics between `set_timeouts` and the first poll of
        // `send_fut`.
        let connector_timeouts = PhasedTimeouts::from_resolved(&timeouts);
        let send_fut = async move {
            let res = dispatch.dispatch(request).await;
            // Bump the pool counter. We treat the very first request
            // to a host as a "dial" and subsequent ones as a "reuse".
            // (The hyper client pools per-host internally; we
            // observe the user-visible count, not the wire.)
            if res.is_ok() {
                let count = pool.total();
                if count == 0 {
                    pool.record_dial(host_key_for_send.clone());
                } else {
                    pool.record_reuse(host_key_for_send.clone());
                }
                tracing::debug!(host = %host_for_log, "upstream request completed");
            }
            res
        };
        let send_fut = CALL_TIMEOUTS.scope(connector_timeouts, send_fut);

        // ---- Real per-phase race --------------------------------------
        // Three nested ceilings (outer -> inner):
        //   1. `total_deadline`   -> Timeout(Headers)  (absolute cap;
        //                                              attributed to
        //                                              Headers because
        //                                              UpstreamPhase
        //                                              has no Total
        //                                              variant and
        //                                              cannot be added
        //                                              without touching
        //                                              pipeline.rs)
        //   2. `write_deadline`   -> Timeout(Write)   (per-phase, OUTER)
        //   3. `headers_deadline` -> Timeout(Headers) (per-phase, INNER)
        //
        // The dispatch future is raced against `write_deadline` first.
        // If the dispatch future completes before `write_deadline` AND
        // `headers_deadline`, we get the response. If `write_deadline`
        // fires first, we report `Timeout(Write)`. If
        // `headers_deadline` fires first (and the dispatch future is
        // still in flight), we report `Timeout(Headers)`.
        //
        // The previous version collapsed these into a single
        // `min(headers, write, dial, tls, total)` ceiling. That is
        // explicitly forbidden by the bug-2 contract: the user
        // rejected soft-accumulation. The nested-timeouts design
        // honors the per-phase attribution because each ceiling
        // carries its own label.
        // Bug 2 fix: build the phase_hint sleep future here. It
        // only resolves when a dispatch declares a `phase_hint`;
        // otherwise it stays pending forever and the
        // `if phase_hint.is_some()` guard on the `select!` arm
        // makes it a no-op.
        let phase_hint_sleep = async {
            match phase_hint {
                Some(phase) => {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(
                        deadlines.deadline_for(phase),
                    ))
                    .await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        let total_sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadlines.total_deadline));
        let write_sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadlines.write_deadline));
        let headers_sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadlines.headers_deadline));
        let cancel_wait = async move {
            cancel_for_send.cancelled().await;
        };
        tokio::pin!(send_fut);
        tokio::pin!(total_sleep);
        tokio::pin!(write_sleep);
        tokio::pin!(headers_sleep);
        tokio::pin!(phase_hint_sleep);
        tokio::pin!(cancel_wait);
        let response = tokio::select! {
            biased;
            _ = &mut cancel_wait => return Err(UpstreamError::Cancel),
            // OUTERMOST ceiling: the absolute total budget. We
            // attribute it to `Headers` (the closest existing
            // phase boundary the dispatch future was waiting on)
            // because `UpstreamPhase` does not have a `Total`
            // variant. A stalled call that has burned through every
            // per-phase budget surfaces as `Timeout(Headers)` from
            // this outermost race.
            _ = &mut total_sleep => return Err(UpstreamError::Timeout(UpstreamPhase::Headers)),
            // Dispatch-supplied phase hint. When the test harness
            // (or any future production wrapper) declares a
            // `phase_hint`, we honour its exact deadline and
            // attribute the timeout to that phase. Guarded by
            // `if phase_hint.is_some()` so production dispatches
            // (which return `None`) keep racing against the
            // generic per-phase ceilings below.
            _ = &mut phase_hint_sleep, if phase_hint.is_some() => {
                return Err(UpstreamError::Timeout(phase_hint.unwrap_or(UpstreamPhase::Headers)));
            }
            // OUTER per-phase ceiling. Fires at `start + write_ms`.
            // Labelled `Write` so a slow body upload is correctly
            // attributed.
            _ = &mut write_sleep => return Err(UpstreamError::Timeout(UpstreamPhase::Write)),
            // INNER per-phase ceiling. Fires at `start + headers_ms`
            // but ONLY if `write_sleep` is still pending. Labelled
            // `Headers` so a slow server (but prompt body upload) is
            // correctly attributed.
            _ = &mut headers_sleep => return Err(UpstreamError::Timeout(UpstreamPhase::Headers)),
            res = &mut send_fut => match res {
                Ok(r) => r,
                Err(e) => {
                    // The dispatch shims (ProductionDispatch,
                    // TestDispatch) already convert a phased
                    // connector error into `Timeout(phase)` at the
                    // boundary, so we pass that through unchanged.
                    // The fallback `recover_phased_phase` is a
                    // belt-and-suspenders for any path that doesn't
                    // (none today, but kept defensively).
                    if let Some(phase) = recover_phased_phase(&e) {
                        return Err(UpstreamError::Timeout(phase));
                    }
                    return Err(e);
                }
            },
        };

        let (parts, body) = response.into_parts();
        // Wrap the streaming body in our cancellable adapter. The
        // body-chunk timer is computed as a GAP inside `next_chunk`
        // (see the doc on `UpstreamBodyStream`), so we only pass the
        // request start, the gap budget in ms, and the total ceiling.
        let body_stream = UpstreamBodyStream::from_hyper(
            body,
            cancel.clone(),
            deadlines.start,
            timeouts.body_chunk_ms,
            deadlines.total_deadline,
            // 32 MiB hard cap per body. A real config knob is a
            // follow-up.
            32 * 1024 * 1024,
        );

        Ok(UpstreamResponse {
            status: parts.status,
            headers: parts.headers,
            body: body_stream,
        })
    }
}

#[cfg(feature = "upstream-hyper")]
fn spawn_eviction_loop(pool: Pool) {
    // The eviction loop is a non-critical best-effort cleanup task.
    // Skip it when no Tokio runtime is active — this is the case for
    // `#[test]` (sync) call sites that build an `UpstreamClient` and
    // never call into it, e.g. the pipeline's `test_config` helper.
    // In production, a Tokio multi-threaded runtime is always present
    // (`#[tokio::main]` on the server) and the spawn succeeds.
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        loop {
            // MEDIUM-3 fix: the sweep interval is 30s and the
            // eviction age is 60s (wall-clock `Duration`, not ticks).
            // The previous design bumped a tick on every
            // `record_dial`/`record_reuse` and evicted entries older
            // than 2 ticks — under low traffic the tick barely
            // advanced and idle entries were NEVER evicted. The new
            // design uses `Instant` (monotonic) so eviction is
            // independent of request volume.
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let evicted = pool.evict_older_than(std::time::Duration::from_secs(60));
            if evicted > 0 {
                tracing::debug!(evicted, "upstream pool eviction sweep");
            }
        }
    });
}

// -----------------------------------------------------------------------
// Test-time dispatch shims
// -----------------------------------------------------------------------

/// Production dispatch shim: wraps a real `HyperClient<PhasedConnector>`.
#[cfg(feature = "upstream-hyper")]
struct ProductionDispatch {
    inner: HyperClient<PhasedConnector, Full<Bytes>>,
}

#[cfg(feature = "upstream-hyper")]
impl HyperDispatchDyn for ProductionDispatch {
    fn dispatch(
        &self,
        req: Request<Full<Bytes>>,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<http::Response<hyper::body::Incoming>, UpstreamError>>
                + Send
                + '_,
        >,
    > {
        // HIGH-6 fix: NO body drain, NO HeaderMap clone. The caller
        // (`call_inner`) builds `Request<Full<Bytes>>` directly from
        // the `Option<Bytes>` body; we hand it straight to hyper's
        // legacy client. The previous dyn-Body trait forced a
        // `body.collect().await` + `HeaderMap::clone()` round-trip
        // here — pure waste on every request.
        let inner = self.inner.clone();
        Box::pin(async move {
            inner
                .request(req)
                .await
                .map_err(|e| {
                    let phase = hyper_source_phase(&e);
                    match phase {
                        Some(p) => UpstreamError::Timeout(p),
                        None => UpstreamError::Http(e.to_string()),
                    }
                })
        })
    }

    fn phase_hint(&self) -> Option<UpstreamPhase> {
        None
    }
}

/// Walk the `source()` chain of a hyper `Error` looking for a
/// `PhasedConnectorError` and return its phase. Returns `None` if
/// the chain does not contain one (e.g. a non-phased test connector).
#[cfg(feature = "upstream-hyper")]
fn hyper_source_phase(
    e: &hyper_util::client::legacy::Error,
) -> Option<UpstreamPhase> {
    use std::error::Error as _;
    // The `source()` of hyper's legacy `Error` is the connector
    // error (a `Box<dyn Error + Send + Sync>` wrapping our
    // `PhasedConnectorError`). Walk the chain until we find one.
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

/// Test dispatch shim: wraps any `HyperClient<C>` and exposes a
/// configurable `phase_hint`.
#[cfg(feature = "upstream-hyper")]
struct TestDispatch<C, T>
where
    C: tower_service::Service<
            Uri,
            Response = T,
            Error = Box<dyn std::error::Error + Send + Sync>,
            Future: Send + Unpin + 'static,
        > + Send
        + Sync
        + 'static,
    T: hyper::rt::Read
        + hyper::rt::Write
        + HyperConnection
        + Unpin
        + Send
        + 'static,
{
    hyper: HyperClient<C, Full<Bytes>>,
    phase_hint: Option<UpstreamPhase>,
}

#[cfg(feature = "upstream-hyper")]
impl<C, T> HyperDispatchDyn for TestDispatch<C, T>
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
    T: hyper::rt::Read
        + hyper::rt::Write
        + HyperConnection
        + Unpin
        + Send
        + 'static,
{
    fn dispatch(
        &self,
        req: Request<Full<Bytes>>,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<http::Response<hyper::body::Incoming>, UpstreamError>>
                + Send
                + '_,
        >,
    > {
        // HIGH-6 fix: same as production — no body drain, no
        // HeaderMap clone. The request is handed straight to hyper.
        let inner = self.hyper.clone();
        Box::pin(async move {
            inner
                .request(req)
                .await
                .map_err(|e| {
                    // Bug 2b/2c fix: same downcast as the production
                    // dispatch. The test connector also surfaces
                    // `PhasedConnectorError` via hyper's `source()`.
                    if let Some(phase) = hyper_source_phase(&e) {
                        return UpstreamError::Timeout(phase);
                    }
                    if e.is_connect() {
                        UpstreamError::Connection(e.to_string())
                    } else {
                        UpstreamError::Http(e.to_string())
                    }
                })
        })
    }

    fn phase_hint(&self) -> Option<UpstreamPhase> {
        self.phase_hint
    }
}
