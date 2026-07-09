//! Real per-phase connector for the `upstream/` client.
//!
//! ## Why this file exists (bug 2b/2c fix)
//!
//! `hyper_util::client::legacy::Client::request` is a single future that
//! collapses DNS, dial, TLS, write, and wait-for-headers into one. The
//! previous version of this module worked around that by picking
//! `min(headers_ms, write_ms, dial_ms, tls_ms, total_ms)` as the
//! effective deadline of that single future and labelling every timeout
//! as `Timeout(Headers)`. That is "soft-accumulation" — a `write_ms =
//! 200ms` config cap on the body upload never produced `Timeout(Write)`
//! because hyper never told us where the body upload stopped and the
//! wait-for-headers started.
//!
//! This module replaces that workaround with **real per-phase
//! enforcement**:
//!
//! - **DNS**, **Dial**, **TLS** are enforced INSIDE the connector with
//!   independent `tokio::time::timeout` calls. A stalled DNS lookup
//!   fires `Timeout(Dns)`, a stalled dial fires `Timeout(Dial)`, a
//!   stalled TLS handshake fires `Timeout(Tls)`. The connector reports
//!   the stalled phase to the upper layer via a `PhasedConnectorError`
//!   downcast on the boxed error.
//!
//! - **Write** vs **Headers** are separated with a NESTED
//!   `tokio::time::timeout` in `client::call_inner`. The outer race
//!   has `write_ms` and reports `Timeout(Write)`; the inner race has
//!   `headers_ms` and reports `Timeout(Headers)`. Whichever ceiling
//!   fires first wins. With `write_ms=200` and `headers_ms=30000` the
//!   outer race fires first and the caller sees `Timeout(Write)` —
//!   which is the contract the previous version silently violated.
//!
//! - **Total** is the outermost ceiling.
//!
//! ## TLS
//!
//! The production HTTPS path upgrades the `TcpStream` to TLS via
//! `tokio_rustls::TlsConnector::connect`, bounded by `timeouts.tls`.
//! The per-phase timeout infrastructure is fully wired up; this module
//! is the only place TLS is configured. The HTTP path stays plain
//! `TcpStream` and skips this step entirely. The `PhasedConnection`
//! enum below holds either shape; both satisfy hyper-util's
//! `Connect` blanket impl (`Read + Write + Connection + Unpin + Send`).
//!
//! Historical note: an earlier revision of this module used a
//! no-op `tls_handshake` placeholder that returned `Ok(())` for
//! HTTPS URIs, with a `// TODO (gate 1+)` comment. The symptom was
//! that hyper wrote a plaintext `HTTP/1.1` request line on top of
//! the unencrypted TCP socket, and the upstream (e.g. NVIDIA NIM)
//! replied with `400 The plain HTTP request was sent to HTTPS port`.
//! Every HTTPS upstream failed. The fix is the real
//! `tokio-rustls` integration below.
//!
//! ## Why a custom connector (not wrap-the-future)
//!
//! The alternative ("wrap the hyper `Service::call` future and
//! `tokio::select!` on progress events") was considered and rejected:
//! hyper-util's `legacy::Client` does not expose progress events for
//! its internal `Service::call`, so any wrapper would be a best-effort
//! `tokio::time::timeout` on the whole future — which is exactly the
//! soft-accumulation we are trying to fix. A custom connector
//! implementing `tower_service::Service<Uri>` is the only way to get
//! real per-step deadlines.

use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use http::Uri;
use hyper::rt::{Read, Write};
use hyper_util::client::legacy::connect::Connection as HyperConnection;
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio_rustls::{TlsConnector, client::TlsStream as ClientTlsStream};
use tower_service::Service;

use super::phases::UpstreamPhase;

/// Returns `true` for private, reserved, loopback, and link-local IP
/// addresses that should never be the target of an upstream HTTP request
/// (SSRF protection).
pub fn is_private_or_reserved(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.octets()[0] == 0
                || v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
        }
    }
}

/// The connection type returned by the connector. Plain HTTP keeps a
/// `TokioIo<TcpStream>`; HTTPS wraps it in `TokioIo<ClientTlsStream<TcpStream>>`.
/// Both variants satisfy hyper-util's `Connect` blanket impl bounds
/// (`Read + Write + Connection + Unpin + Send + 'static`).
pub enum PhasedConnection {
    Plain(TokioIo<TcpStream>),
    /// The `bool` is `true` when ALPN negotiated `h2` (HTTP/2), `false`
    /// when the server picked `http/1.1` (or ALPN was not offered).
    /// `connected()` reads this flag to tell hyper-util whether to use
    /// the HTTP/2 or HTTP/1.1 protocol parser — getting this wrong
    /// produces `invalid HTTP version parsed` errors at 6ms.
    Tls {
        io: Box<TokioIo<ClientTlsStream<TcpStream>>>,
        negotiated_h2: bool,
    },
}

impl Read for PhasedConnection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<Result<(), io::Error>> {
        match &mut *self {
            PhasedConnection::Plain(io) => Pin::new(io).poll_read(cx, buf),
            PhasedConnection::Tls { io, .. } => Pin::new(&mut **io).poll_read(cx, buf),
        }
    }
}

impl Write for PhasedConnection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        match &mut *self {
            PhasedConnection::Plain(io) => Pin::new(io).poll_write(cx, buf),
            PhasedConnection::Tls { io, .. } => Pin::new(&mut **io).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match &mut *self {
            PhasedConnection::Plain(io) => Pin::new(io).poll_flush(cx),
            PhasedConnection::Tls { io, .. } => Pin::new(&mut **io).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        match &mut *self {
            PhasedConnection::Plain(io) => Pin::new(io).poll_shutdown(cx),
            PhasedConnection::Tls { io, .. } => Pin::new(&mut **io).poll_shutdown(cx),
        }
    }
}

/// HTTP connection metadata. Reports the negotiated ALPN protocol
/// so hyper-util can select HTTP/2 or HTTP/1.1 as appropriate.
impl HyperConnection for PhasedConnection {
    fn connected(&self) -> hyper_util::client::legacy::connect::Connected {
        match self {
            PhasedConnection::Tls { negotiated_h2, .. } => {
                // Bug fix: report the ALPN-negotiated protocol to
                // hyper-util. The TLS connector offers `h2` +
                // `http/1.1` via ALPN; if the server picks `h2`, we
                // MUST tell hyper-util via `negotiated_h2()` so it
                // uses the HTTP/2 protocol parser. Without this,
                // hyper-util assumes HTTP/1.1, tries to parse an
                // HTTP/2 response as HTTP/1.1, and fails at ~6ms
                // with `invalid HTTP version parsed`.
                let mut connected = hyper_util::client::legacy::connect::Connected::new();
                if *negotiated_h2 {
                    connected = connected.negotiated_h2();
                }
                connected
            }
            PhasedConnection::Plain(_) => hyper_util::client::legacy::connect::Connected::new(),
        }
    }
}

/// A process-wide `TlsConnector` configured with webpki roots. The
/// rustls `ClientConfig` is cheap to clone (internally `Arc`) and is
/// shared across every HTTPS request. Loading webpki roots is a few
/// KB and happens once at first use.
fn tls_connector() -> TlsConnector {
    static CONFIG: std::sync::OnceLock<Arc<rustls::ClientConfig>> = std::sync::OnceLock::new();
    let cfg = CONFIG.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let mut config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        // Enable ALPN h2 + http/1.1 so the server can negotiate HTTP/2.
        // Without ALPN, even with hyper's http2 feature, the server
        // falls back to HTTP/1.1.
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Arc::new(config)
    });
    TlsConnector::from(cfg.clone())
}

/// Per-phase timeouts carried by a `PhasedConnector`. All values are
/// "max duration" for the corresponding phase; the connector enforces
/// them with `tokio::time::timeout` and reports the stalled phase on
/// expiry.
#[derive(Debug, Clone, Copy)]
pub struct PhasedTimeouts {
    pub dns: Duration,
    pub dial: Duration,
    pub tls: Duration,
}

impl PhasedTimeouts {
    /// Build from the `ResolvedTimeouts` of a `TimeoutProfile`.
    pub fn from_resolved(t: &super::profile::ResolvedTimeouts) -> Self {
        Self {
            dns: Duration::from_millis(t.dns_ms),
            dial: Duration::from_millis(t.dial_ms),
            tls: Duration::from_millis(t.tls_ms),
        }
    }
}

impl Default for PhasedTimeouts {
    /// Conservative defaults: 5s for each phase (matches the
    /// `SYSTEM_DEFAULTS` in `profile.rs`).
    fn default() -> Self {
        Self {
            dns: Duration::from_secs(5),
            dial: Duration::from_secs(5),
            tls: Duration::from_secs(5),
        }
    }
}

/// Errors surfaced by `PhasedConnector::call`. Implements
/// `std::error::Error + Send + Sync` (the trait bounds the hyper-util
/// `Connect` blanket impl demands) and carries a `phase` so the upper
/// layer (`client::call_inner`) can attribute a timeout to the right
/// step. Downcasting `Box<dyn Error + Send + Sync>` to this type
/// recovers the phase.
#[derive(Debug)]
pub struct PhasedConnectorError {
    pub phase: UpstreamPhase,
    pub kind: PhasedErrorKind,
}

#[derive(Debug)]
pub enum PhasedErrorKind {
    /// The corresponding phase exceeded its deadline.
    Timeout,
    /// The connector rejected the URI (unsupported scheme, missing
    /// host, etc.).
    InvalidUri(String),
    /// Lower-level I/O failure (DNS resolution, TCP, TLS).
    Io(io::Error),
}

impl std::fmt::Display for PhasedConnectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            PhasedErrorKind::Timeout => {
                write!(f, "phased connector: timeout in phase `{}`", self.phase)
            }
            PhasedErrorKind::InvalidUri(s) => write!(
                f,
                "phased connector: invalid URI in phase `{}`: {}",
                self.phase, s
            ),
            PhasedErrorKind::Io(e) => write!(
                f,
                "phased connector: I/O error in phase `{}`: {}",
                self.phase, e
            ),
        }
    }
}

impl std::error::Error for PhasedConnectorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            PhasedErrorKind::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// A `tower::Service<Uri>` connector that enforces DNS, dial, and TLS
/// timeouts independently and reports the stalled phase on error.
///
/// ## How it separates phases
///
/// `PhasedConnector::call(uri)` parses the URI, resolves the hostname
/// to one or more socket addresses (DNS phase, time-bounded by
/// `timeouts.dns`), dials the first address that succeeds (Dial phase,
/// time-bounded by `timeouts.dial`), and (for `https`) wraps the
/// resulting TCP stream in a TLS handshake (Tls phase, time-bounded
/// by `timeouts.tls`). Each phase is a separate `tokio::time::timeout`;
/// on expiry the future resolves to
/// `Err(PhasedConnectorError { phase, Timeout })`.
///
/// The `HttpConnector` from `hyper-util` collapses all three phases
/// into a single future, which is why we don't reuse it: we want the
/// per-phase attribution. We still reuse `hyper_util::rt::TokioIo` as
/// the `Read + Write + Connection` wrapper, which is the only piece
/// the hyper-util `Connect` blanket impl needs from us.
///
/// A `tower::Service<Uri>` connector that enforces DNS, dial, and TLS
/// timeouts independently and reports the stalled phase on error.
///
/// See the `CALL_TIMEOUTS` task-local below for the per-call timeout
/// injection mechanism (HIGH-5 fix).
#[derive(Clone)]
pub struct PhasedConnector {
    /// Fallback timeouts used when the `CALL_TIMEOUTS` task-local is
    /// not set (e.g. tests that build a `PhasedConnector` directly).
    /// Production paths always set the task-local via
    /// `UpstreamClient::call_inner`.
    defaults: PhasedTimeouts,
}

// Per-call timeout injection (HIGH-5 fix)
//
// The hyper-util `legacy::Client` clones its connector for each
// request, so the connector is a `Clone` value that is **shared**
// across concurrent calls. We don't have a per-call setup hook
// (hyper-util calls `Service::call` directly on the cloned
// connector), so we cannot thread the per-call timeouts into
// `call()` by argument.
//
// Previous design (RACE): the per-phase deadlines were stored in
// `Arc<AtomicU64>` fields shared across every concurrent request that
// borrowed the same `UpstreamClient`. The caller wrote the timeouts
// via `set_timeouts(...)` immediately before polling the dispatch
// future, but `tokio::select!` does not poll that future synchronously
// — between `set_timeouts` and the first poll, another request's
// `call_inner` could call `set_timeouts` and clobber the atomics. The
// race window was tiny but real, and under high concurrency one
// request could inherit another request's per-phase budget.
//
// Current design (RACE-FREE): a `tokio::task_local!` slot
// (`CALL_TIMEOUTS`) carries the per-call `PhasedTimeouts` from
// `UpstreamClient::call_inner` down to `PhasedConnector::call`. The
// caller wraps the dispatch future in `CALL_TIMEOUTS.scope(value,
// future)`; the connector reads the slot via `try_with` and falls
// back to its stored `defaults` if the slot is unset. Each task has
// its own slot, no shared mutable state, no clobbering.
//
// The `defaults` field is kept for tests that build a `PhasedConnector`
// directly without going through `UpstreamClient::call_inner`. In
// production, the task-local is always set before the connector's
// `call()` is polled.
tokio::task_local! {
    pub(crate) static CALL_TIMEOUTS: PhasedTimeouts;
}
tokio::task_local! {
    pub static CALL_PROXY: Option<String>;
}

impl PhasedConnector {
    /// Build a connector with the given per-phase timeouts (used as
    /// the fallback when the `CALL_TIMEOUTS` task-local is unset).
    pub fn new(timeouts: PhasedTimeouts) -> Self {
        Self { defaults: timeouts }
    }

    /// Build a connector with the system default timeouts (5s each).
    pub fn with_defaults() -> Self {
        Self::new(PhasedTimeouts::default())
    }

    /// Read the effective per-phase timeouts. Checks the `CALL_TIMEOUTS`
    /// task-local first (set by `UpstreamClient::call_inner`); falls
    /// back to the stored `defaults` if the slot is unset.
    ///
    /// This replaces the old `set_timeouts` + `timeouts()` pair. The
    /// caller no longer needs to write atomics before issuing the
    /// request — the task-local is set once per call via `scope(...)`
    /// and read here.
    pub fn effective_timeouts(&self) -> PhasedTimeouts {
        CALL_TIMEOUTS.try_with(|t| *t).unwrap_or(self.defaults)
    }

    /// Backward-compat: set the fallback timeouts. Kept for any test
    /// that calls `set_timeouts` directly; production code should use
    /// the task-local via `UpstreamClient::call_inner`.
    pub fn set_timeouts(&self, _timeouts: PhasedTimeouts) {
        // No-op: the per-call timeouts are now passed via the
        // `CALL_TIMEOUTS` task-local. This method is kept only for
        // source compatibility with tests that called it directly.
        // The `defaults` are NOT mutated because the connector is
        // shared across concurrent requests via `Clone` — mutating
        // `defaults` would re-introduce the race we just fixed.
    }

    /// Backward-compat: read the fallback timeouts (NOT the per-call
    /// task-local). Kept for the `Debug` impl. Production code should
    /// use `effective_timeouts()` instead.
    pub fn timeouts(&self) -> PhasedTimeouts {
        self.defaults
    }
}

impl std::fmt::Debug for PhasedConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhasedConnector")
            .field("timeouts", &self.timeouts())
            .finish()
    }
}

impl Service<Uri> for PhasedConnector {
    type Response = PhasedConnection;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    // The hyper-util `Connect` blanket impl requires
    // `S::Future: Unpin + Send`. We use a trait object `Pin<Box<dyn
    // Future + Send>>`: this is `Unpin` for ANY inner type (because
    // `Pin<Box<T>>: Unpin` regardless of `T: Unpin`), so the inner
    // async block — which awaits non-`Unpin` futures like
    // `tokio::time::Timeout` and `TcpStream::connect` — does NOT need
    // to itself be `Unpin`.
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // We are always ready: the per-call future does not share
        // state with `poll_ready` (no rate limit, no resolver pool,
        // no connect semaphore). This matches the `HttpConnector`
        // behavior in `hyper-util` for the common case where the
        // resolver is `GaiResolver`.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        // HIGH-5 fix: read the per-call timeouts from the task-local
        // (set by `UpstreamClient::call_inner` via `CALL_TIMEOUTS.scope`).
        // Falls back to `defaults` if the slot is unset (tests).
        let timeouts = self.effective_timeouts();
        let is_https = uri.scheme_str() == Some("https");
        // See the comment on `type Future` for why we don't write
        // `+ Unpin` here: the inner async block is not `Unpin`
        // (it awaits `tokio::time::Timeout`), but the boxed trait
        // object IS.
        Box::pin(run_phased_connect(uri, is_https, timeouts))
    }
}

/// The actual connect future. Pulled out as a free function so the
/// `Service::call` signature stays simple.
async fn run_phased_connect(
    uri: Uri,
    is_https: bool,
    timeouts: PhasedTimeouts,
) -> Result<PhasedConnection, Box<dyn std::error::Error + Send + Sync>> {
    // ---- Connect budget (accumulative) --------------------------------
    // CRITICAL FIX: the connect phases (DNS + Dial + TLS) share a SINGLE
    // accumulative budget derived from `connect_ms` (the largest of
    // dns/dial/tls). Each phase gets only the REMAINING time from the
    // budget, NOT a fresh `timeouts.dns` / `timeouts.dial` / `timeouts.tls`
    // window from zero.
    //
    // BUG: the previous code used `tokio::time::timeout(timeouts.dns, ...)`,
    // `tokio::time::timeout(timeouts.dial, ...)`, `tokio::time::timeout(timeouts.tls, ...)`
    // — each with its OWN independent window. If DNS took 3.5s, Dial got
    // a fresh 7s window (not 7s - 3.5s = 3.5s), and TLS got another fresh
    // 7s window. The total connect time could be up to 17.5s even though
    // the global `connect_ms` was 7000ms. This caused "client disconnected"
    // errors at ~11s because the client (or an intermediate proxy) timed
    // out waiting for the first byte while openproxy was still in the TLS
    // handshake.
    //
    // The fix: compute a single `connect_deadline` = start + max(dns, dial, tls).
    // Each phase's timeout is `connect_deadline - now` (clamped to >= 1ms).
    // This ensures the TOTAL connect time never exceeds the configured
    // `connect_ms` budget, regardless of how the time is split across
    // phases.
    let connect_start = std::time::Instant::now();
    let connect_budget = timeouts.dns.max(timeouts.dial).max(timeouts.tls);
    let connect_deadline = connect_start + connect_budget;

    // ---- Parse the URI ---------------------------------------------------
    let (host, port) = match parse_authority(&uri) {
        Ok(v) => v,
        Err(msg) => {
            return Err(Box::new(PhasedConnectorError {
                phase: UpstreamPhase::Dns,
                kind: PhasedErrorKind::InvalidUri(msg),
            }));
        }
    };

    // ---- Resolve Proxy Config --------------------------------------------
    let proxy_opt = CALL_PROXY.try_with(|p| p.clone()).unwrap_or(None);
    let mut proxy_config_opt = None;
    if let Some(ref proxy_url) = proxy_opt {
        match parse_proxy_url(proxy_url) {
            Ok(cfg) => proxy_config_opt = Some(cfg),
            Err(e) => {
                return Err(Box::new(PhasedConnectorError {
                    phase: UpstreamPhase::Dns,
                    kind: PhasedErrorKind::InvalidUri(format!("Invalid proxy config: {}", e)),
                }));
            }
        }
    }

    let dial_host = if let Some(ref proxy) = proxy_config_opt {
        proxy.host.clone()
    } else {
        host.clone()
    };
    let dial_port = if let Some(ref proxy) = proxy_config_opt {
        proxy.port
    } else {
        port
    };

    // ---- Phase 1: DNS ---------------------------------------------------
    // If `dial_host` is already a literal IP, skip DNS (and attribute any
    // later timeout to `Dial`, not `Dns`).
    let addrs: Vec<SocketAddr> = if let Some(literal) = parse_literal_ip(&dial_host, dial_port) {
        vec![literal]
    } else {
        let dns_remaining = connect_deadline
            .checked_duration_since(std::time::Instant::now())
            .unwrap_or(std::time::Duration::from_millis(0));
        let dns_timeout = timeouts.dns.min(dns_remaining);
        if dns_timeout.is_zero() {
            return Err(Box::new(PhasedConnectorError {
                phase: UpstreamPhase::Dns,
                kind: PhasedErrorKind::Timeout,
            }));
        }
        match tokio::time::timeout(dns_timeout, resolve_host(&dial_host, dial_port)).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                return Err(Box::new(PhasedConnectorError {
                    phase: UpstreamPhase::Dns,
                    kind: PhasedErrorKind::Io(e),
                }));
            }
            Err(_) => {
                return Err(Box::new(PhasedConnectorError {
                    phase: UpstreamPhase::Dns,
                    kind: PhasedErrorKind::Timeout,
                }));
            }
        }
    };

    // ---- SSRF filter: skip private / reserved addresses --------------------
    // Cloud metadata endpoints (169.254.169.254), loopback, RFC-1918,
    // and link-local ranges must never be the target of an upstream request.
    //
    // In test builds the filter is disabled so integration tests can use
    // localhost mock servers without being blocked.
    let allow_private = cfg!(test)
        || cfg!(feature = "ssrf-bypass")
        || std::env::var("OPENPROXY_ALLOW_PRIVATE_UPSTREAMS")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);

    let addrs: Vec<SocketAddr> = if allow_private {
        if cfg!(test) {
            tracing::debug!("SSRF: filter disabled in test build");
        } else {
            tracing::debug!("SSRF: private upstream connections allowed via OPENPROXY_ALLOW_PRIVATE_UPSTREAMS");
        }
        addrs
    } else {
        let filtered: Vec<SocketAddr> = addrs
            .into_iter()
            .filter(|a| {
                if is_private_or_reserved(&a.ip()) {
                    tracing::warn!(addr = %a, "SSRF: skipping private/reserved address");
                    false
                } else {
                    true
                }
            })
            .collect();

        if filtered.is_empty() {
            return Err(Box::new(PhasedConnectorError {
                phase: UpstreamPhase::Dial,
                kind: PhasedErrorKind::Io(io::Error::other(
                    "all resolved addresses are private/reserved (SSRF block). Set OPENPROXY_ALLOW_PRIVATE_UPSTREAMS=true to allow.",
                )),
            }));
        }
        filtered
    };

    // ---- Phase 2: Dial --------------------------------------------------
    // Try the addresses in order. The first one that succeeds wins; if
    // all fail with I/O errors, return the last one. A connect-budget
    // expiry on any single attempt is reported as `Timeout(Dial)` so
    // a stuck connect is correctly attributed.
    let mut last_err: Option<io::Error> = None;
    let mut stream: Option<TcpStream> = None;
    for addr in addrs {
        let dial_remaining = connect_deadline
            .checked_duration_since(std::time::Instant::now())
            .unwrap_or(std::time::Duration::from_millis(0));
        let dial_timeout = timeouts.dial.min(dial_remaining);
        if dial_timeout.is_zero() {
            return Err(Box::new(PhasedConnectorError {
                phase: UpstreamPhase::Dial,
                kind: PhasedErrorKind::Timeout,
            }));
        }
        match tokio::time::timeout(dial_timeout, TcpStream::connect(addr)).await {
            Ok(Ok(s)) => {
                stream = Some(s);
                break;
            }
            Ok(Err(e)) => last_err = Some(e),
            Err(_) => {
                return Err(Box::new(PhasedConnectorError {
                    phase: UpstreamPhase::Dial,
                    kind: PhasedErrorKind::Timeout,
                }));
            }
        }
    }
    let mut stream = match stream {
        Some(s) => s,
        None => {
            return Err(Box::new(PhasedConnectorError {
                phase: UpstreamPhase::Dial,
                kind: PhasedErrorKind::Io(
                    last_err.unwrap_or_else(|| io::Error::other("no addresses to dial")),
                ),
            }));
        }
    };

    // ---- Proxy Handshake Tunnel -----------------------------------------
    if let Some(ref proxy_config) = proxy_config_opt {
        let dial_remaining = connect_deadline
            .checked_duration_since(std::time::Instant::now())
            .unwrap_or(std::time::Duration::from_millis(0));
        if dial_remaining.is_zero() {
            return Err(Box::new(PhasedConnectorError {
                phase: UpstreamPhase::Dial,
                kind: PhasedErrorKind::Timeout,
            }));
        }

        match tokio::time::timeout(
            dial_remaining,
            run_proxy_tunnel(stream, proxy_config, &host, port),
        )
        .await
        {
            Ok(Ok(s)) => {
                stream = s;
            }
            Ok(Err(e)) => {
                return Err(Box::new(PhasedConnectorError {
                    phase: UpstreamPhase::Dial,
                    kind: PhasedErrorKind::Io(io::Error::other(format!(
                        "Proxy handshake failed: {}",
                        e
                    ))),
                }));
            }
            Err(_) => {
                return Err(Box::new(PhasedConnectorError {
                    phase: UpstreamPhase::Dial,
                    kind: PhasedErrorKind::Timeout,
                }));
            }
        }
    }

    // Best-effort nodelay + TCP keepalive. Failure here is not fatal
    // (a slow consumer is hyper's problem, not ours).
    //
    // Bug fix (SendRequest at 6-9ms): TCP keepalive probes detect
    // stale connections at the kernel level. Without keepalive, a
    // connection that the server closed silently (no FIN/RST
    // received) looks healthy to the client until the next write
    // fails. With keepalive, the kernel probes every 30s (idle) and
    // drops the socket after 3 failed probes (~90s), so hyper's pool
    // never hands out a stale connection.
    let _ = stream.set_nodelay(true);
    {
        let sock = socket2::SockRef::from(&stream);
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(std::time::Duration::from_secs(30))
            .with_interval(std::time::Duration::from_secs(10));
        let _ = sock.set_tcp_keepalive(&keepalive);
    }

    // ---- Phase 3: TLS ---------------------------------------------------
    // Real `tokio-rustls` handshake. The connector is process-wide
    // (rustls `ClientConfig` is `Arc`-backed). The handshake is bounded
    // by the REMAINING connect budget (not a fresh `timeouts.tls` window);
    // a stalled or rejected TLS handshake surfaces as `Timeout(Tls)` or
    // `Io(Tls)`.
    if is_https {
        let server_name = match ServerName::try_from(host.clone()) {
            Ok(n) => n,
            Err(e) => {
                return Err(Box::new(PhasedConnectorError {
                    phase: UpstreamPhase::Tls,
                    kind: PhasedErrorKind::InvalidUri(format!("bad SNI host: {e}")),
                }));
            }
        };
        let connector = tls_connector();
        let tls_remaining = connect_deadline
            .checked_duration_since(std::time::Instant::now())
            .unwrap_or(std::time::Duration::from_millis(0));
        let tls_timeout = timeouts.tls.min(tls_remaining);
        if tls_timeout.is_zero() {
            return Err(Box::new(PhasedConnectorError {
                phase: UpstreamPhase::Tls,
                kind: PhasedErrorKind::Timeout,
            }));
        }
        match tokio::time::timeout(tls_timeout, connector.connect(server_name, stream)).await {
            Ok(Ok(tls_stream)) => {
                // Bug fix: read the ALPN-negotiated protocol from the
                // rustls ClientConnection BEFORE wrapping the stream
                // in TokioIo (which hides the inner type). If the
                // server picked `h2`, we set `negotiated_h2 = true`
                // so `connected()` can tell hyper-util to use the
                // HTTP/2 parser. Without this, hyper-util defaults
                // to HTTP/1.1 and fails with `invalid HTTP version
                // parsed` when the server responds in HTTP/2.
                let (_, client_conn) = tls_stream.get_ref();
                let negotiated_h2 = client_conn
                    .alpn_protocol()
                    .map(|p| p == b"h2")
                    .unwrap_or(false);
                return Ok(PhasedConnection::Tls {
                    io: Box::new(TokioIo::new(tls_stream)),
                    negotiated_h2,
                });
            }
            Ok(Err(e)) => {
                return Err(Box::new(PhasedConnectorError {
                    phase: UpstreamPhase::Tls,
                    kind: PhasedErrorKind::Io(e),
                }));
            }
            Err(_) => {
                return Err(Box::new(PhasedConnectorError {
                    phase: UpstreamPhase::Tls,
                    kind: PhasedErrorKind::Timeout,
                }));
            }
        }
    }

    Ok(PhasedConnection::Plain(TokioIo::new(stream)))
}

/// `host:port` -> (host, port) with sensible defaults. Returns an
/// error string (not a `PhasedConnectorError`) so the caller can wrap
/// it with the right phase.
fn parse_authority(uri: &Uri) -> Result<(String, u16), String> {
    let host = uri
        .host()
        .ok_or_else(|| "missing host".to_string())?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = uri.port_u16().unwrap_or(match uri.scheme_str() {
        Some("https") => 443,
        _ => 80,
    });
    Ok((host, port))
}

/// If `host` is an IP literal (v4 or v6), build the corresponding
/// `SocketAddr` directly so we can skip the DNS step.
fn parse_literal_ip(host: &str, port: u16) -> Option<SocketAddr> {
    if let Ok(v4) = host.parse::<Ipv4Addr>() {
        Some(SocketAddr::new(IpAddr::V4(v4), port))
    } else if let Ok(v6) = host.parse::<Ipv6Addr>() {
        Some(SocketAddr::new(IpAddr::V6(v6), port))
    } else {
        None
    }
}

/// Resolve `host:port` to one or more `SocketAddr`s using tokio's
/// async DNS, with a simple in-memory cache (5m TTL) to avoid
/// hitting getaddrinfo on every fresh dial.
async fn resolve_host(host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    // Check the DNS cache first. Keyed on (host, port).
    // TTL: 300 seconds (5 minutes).
    // The cache is a process-wide DashMap; entries are (addrs, expiry).
    // On cache hit, return the cached addrs if not expired.
    // On cache miss or expiry, fall through to tokio::net::lookup_host.
    //
    // LEAK FIX: a background sweep (started on first insert) evicts
    // expired entries every 5 minutes. Without this, the cache grows
    // unbounded as the process sees new hosts (provider rotation,
    // CDN shards, etc.) — each entry is ~200 bytes but over weeks of
    // uptime with many providers it adds up.
    static DNS_CACHE: std::sync::OnceLock<
        dashmap::DashMap<String, (Vec<SocketAddr>, std::time::Instant)>,
    > = std::sync::OnceLock::new();
    let cache = DNS_CACHE.get_or_init(dashmap::DashMap::new);
    let cache_key = format!("{}:{}", host, port);
    let now = std::time::Instant::now();
    const DNS_TTL: std::time::Duration = std::time::Duration::from_secs(300);

    if let Some(entry) = cache.get(&cache_key)
        && now < entry.1
    {
        return Ok(entry.0.clone());
    }

    // Cache miss or expired — do the actual DNS lookup.
    let lookup = format!("{}:{}", host, port);
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(lookup).await?.collect();
    // Cache the result (even if empty — an empty result for 60s is
    // better than hammering getaddrinfo on a misconfigured host).
    cache.insert(cache_key, (addrs.clone(), now + DNS_TTL));

    // Start the background eviction sweep on the first successful
    // insert. The sweep runs every 5 minutes and drops entries whose
    // TTL has expired. Idempotent via `OnceLock` on the sweep-started
    // flag — only the first caller spawns the task.
    static SWEEP_STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    SWEEP_STARTED.get_or_init(|| {
        let cache_for_sweep = DNS_CACHE.get().expect("DNS_CACHE init order");
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(300));
            tick.tick().await; // skip immediate first tick
            loop {
                tick.tick().await;
                let now = std::time::Instant::now();
                let before = cache_for_sweep.len();
                cache_for_sweep.retain(|_, (_, expiry)| *expiry > now);
                let after = cache_for_sweep.len();
                if before != after {
                    tracing::debug!(before, after, evicted = before - after, "DNS cache sweep");
                }
            }
        });
    });

    Ok(addrs)
}

// ---------------------------------------------------------------------
// Downcast helper used by `client::call_inner` to recover the phase
// from a boxed connector error.
// ---------------------------------------------------------------------

/// If `err` (or anything in its `source` chain) is a
/// `PhasedConnectorError`, return its phase. Otherwise `None`. The
/// caller falls back to a different attribution (e.g. the legacy
/// `Headers` default) when this returns `None`.
pub fn phased_phase(err: &(dyn std::error::Error + 'static)) -> Option<UpstreamPhase> {
    // Walk the source chain so wrapped errors (e.g. a hyper-util
    // `Connect` wrapping our boxed error) are also detected.
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = current {
        if let Some(p) = e.downcast_ref::<PhasedConnectorError>()
            && matches!(p.kind, PhasedErrorKind::Timeout)
        {
            return Some(p.phase);
        }
        current = e.source();
    }
    None
}

// Compile-time pin: the production `PhasedConnection` (both
// variants) must satisfy the hyper-util `Connect` blanket impl's
// bounds. The blanket impl lives in
// `hyper_util::client::legacy::connect` and is applied to any
// `S::Response` that implements `Read + Write + Connection +
// Unpin + Send + 'static`. We hand-implement `Connection` for
// `PhasedConnection` above; the assertions below make the
// contract statically checkable from the editor (and from CI via
// `cargo check`). Wrapped in an anonymous const block so the
// inner `_assert` is referenced (and thus the bound checks fire)
// without producing a dead_code warning on an uncalled function.
const _: () = {
    fn _assert<R: Read + Write + HyperConnection + Unpin + Send + 'static>() {}
    let _ = _assert::<PhasedConnection>;
    let _ = _assert::<TokioIo<TcpStream>>;
};

#[derive(Debug, Clone)]
struct ProxyConfig {
    scheme: String,
    host: String,
    port: u16,
}

fn parse_proxy_url(url: &str) -> Result<ProxyConfig, String> {
    let uri: http::Uri = url
        .parse()
        .map_err(|e: http::uri::InvalidUri| format!("Invalid proxy URL: {}", e))?;
    let scheme = uri
        .scheme_str()
        .ok_or_else(|| "Missing proxy scheme".to_string())?
        .to_lowercase();
    let host = uri
        .host()
        .ok_or_else(|| "Missing proxy host".to_string())?
        .to_string();
    let port = uri
        .port_u16()
        .ok_or_else(|| "Missing proxy port".to_string())?;
    Ok(ProxyConfig { scheme, host, port })
}

async fn run_proxy_tunnel(
    mut stream: TcpStream,
    proxy: &ProxyConfig,
    dest_host: &str,
    dest_port: u16,
) -> Result<TcpStream, Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    match proxy.scheme.as_str() {
        "socks5" => {
            // 1. Greeting
            stream.write_all(&[0x05, 0x01, 0x00]).await?;
            let mut greeting_resp = [0u8; 2];
            stream.read_exact(&mut greeting_resp).await?;
            if greeting_resp[0] != 0x05 || greeting_resp[1] != 0x00 {
                return Err(
                    io::Error::other("SOCKS5 authentication required or unsupported").into(),
                );
            }

            // 2. Connect request
            let dest_host_bytes = dest_host.as_bytes();
            if let Ok(ip) = dest_host.parse::<std::net::Ipv4Addr>() {
                let mut req = Vec::with_capacity(10);
                req.extend_from_slice(&[0x05, 0x01, 0x00, 0x01]);
                req.extend_from_slice(&ip.octets());
                req.extend_from_slice(&dest_port.to_be_bytes());
                stream.write_all(&req).await?;
            } else if let Ok(ip) = dest_host.parse::<std::net::Ipv6Addr>() {
                let mut req = Vec::with_capacity(22);
                req.extend_from_slice(&[0x05, 0x01, 0x00, 0x04]);
                req.extend_from_slice(&ip.octets());
                req.extend_from_slice(&dest_port.to_be_bytes());
                stream.write_all(&req).await?;
            } else {
                let mut req = Vec::with_capacity(7 + dest_host_bytes.len());
                req.extend_from_slice(&[0x05, 0x01, 0x00, 0x03, dest_host_bytes.len() as u8]);
                req.extend(dest_host_bytes);
                req.extend_from_slice(&dest_port.to_be_bytes());
                stream.write_all(&req).await?;
            }

            // 3. Connect response
            let mut resp_header = [0u8; 4];
            stream.read_exact(&mut resp_header).await?;
            if resp_header[0] != 0x05 || resp_header[1] != 0x00 {
                return Err(io::Error::other(format!(
                    "SOCKS5 connection failed: status={}",
                    resp_header[1]
                ))
                .into());
            }

            // Read address part of the response
            match resp_header[3] {
                0x01 => {
                    // IPv4
                    let mut buf = [0u8; 6];
                    stream.read_exact(&mut buf).await?;
                }
                0x04 => {
                    // IPv6
                    let mut buf = [0u8; 18];
                    stream.read_exact(&mut buf).await?;
                }
                0x03 => {
                    // Domain
                    let len = stream.read_u8().await?;
                    let mut buf = vec![0u8; len as usize + 2];
                    stream.read_exact(&mut buf).await?;
                }
                _ => return Err(io::Error::other("Invalid SOCKS5 address type").into()),
            }
        }
        "socks4" => {
            let ip = dest_host.parse::<std::net::Ipv4Addr>().map_err(|_| {
                io::Error::other("SOCKS4 only supports literal IPv4 destination hosts (use SOCKS5 for hostname resolution)")
            })?;

            let mut req = Vec::with_capacity(9);
            req.extend_from_slice(&[0x04, 0x01]);
            req.extend_from_slice(&dest_port.to_be_bytes());
            req.extend_from_slice(&ip.octets());
            req.push(0x00);
            stream.write_all(&req).await?;

            let mut resp = [0u8; 8];
            stream.read_exact(&mut resp).await?;
            if resp[0] != 0x00 || resp[1] != 0x5a {
                return Err(io::Error::other(format!(
                    "SOCKS4 connection rejected: code={}",
                    resp[1]
                ))
                .into());
            }
        }
        "http" | "https" => {
            let request = format!(
                "CONNECT {}:{} HTTP/1.1\r\nHost: {}:{}\r\nProxy-Connection: Keep-Alive\r\n\r\n",
                dest_host, dest_port, dest_host, dest_port
            );
            stream.write_all(request.as_bytes()).await?;

            let mut headers_buf = Vec::new();
            let mut byte_buf = [0u8; 1];
            loop {
                stream.read_exact(&mut byte_buf).await?;
                headers_buf.push(byte_buf[0]);
                if headers_buf.ends_with(b"\r\n\r\n") {
                    break;
                }
                if headers_buf.len() > 8192 {
                    return Err(io::Error::other("HTTP CONNECT response headers too long").into());
                }
            }

            let resp_str = String::from_utf8_lossy(&headers_buf);
            let first_line = resp_str.split("\r\n").next().unwrap_or("");
            if !first_line.contains(" 200 ") {
                return Err(io::Error::other(format!(
                    "HTTP CONNECT proxy returned error: {}",
                    first_line
                ))
                .into());
            }
        }
        _ => {
            return Err(
                io::Error::other(format!("Unsupported proxy scheme: {}", proxy.scheme)).into(),
            );
        }
    }

    Ok(stream)
}
