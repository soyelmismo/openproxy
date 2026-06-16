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
//! which is the contract the previous version silently violated.
//!
//! - **Total** is the outermost ceiling.
//!
//! ## TLS in Gate 0
//!
//! The TLS step is a structural placeholder that returns success for
//! the production path. The real TLS integration (via
//! `tokio_rustls::TlsConnector`) is a Gate-1 follow-up; the per-phase
//! timeout infrastructure is fully wired up so the Gate 1 integration
//! is a one-line change inside the `tls_handshake` function below. No
//! API surface changes.
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
use std::task::{Context, Poll};
use std::time::Duration;

use http::Uri;
use hyper::rt::{Read, Write};
use hyper_util::client::legacy::connect::Connection as HyperConnection;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tower_service::Service;

use super::phases::UpstreamPhase;

/// The connection type returned by the connector. Always
/// `TokioIo<TcpStream>` in this Gate 0 (TLS is a Gate-1 follow-up that
/// wraps this in a `TokioIo<tokio_rustls::client::TlsStream<TcpStream>>`).
pub type PhasedConnection = TokioIo<TcpStream>;

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
/// ## Per-call timeout injection
///
/// The hyper-util `legacy::Client` clones its connector for each
/// request, so the connector is a `Clone` value that is **shared**
/// across concurrent calls. We don't have a per-call setup hook
/// (hyper-util calls `Service::call` directly on the cloned
/// connector), so we cannot thread the per-call timeouts into
/// `call()` by argument.
///
/// Solution: the per-phase deadlines are stored in `Arc<AtomicU64>`
/// fields. The caller (in `client::call_inner`) writes the
/// per-request timeouts into the atomics just before issuing the
/// request; the connector reads them in its `call` method. The
/// reads are lock-free; the writes are only seen by the current
/// call's `Service::call` invocation (the legacy client polls one
/// request at a time on the cloned connector).
#[derive(Clone)]
pub struct PhasedConnector {
    /// `Arc<AtomicU64>` for each per-phase deadline (millis). All
    /// fields are public-via-`set_timeouts` for `call_inner`'s
    /// setup phase. The atomics are `Ordering::Relaxed` because
    /// they are only used to communicate between the `call_inner`
    /// setup step (one task) and the connector's `call` future
    /// polled by the same task; no cross-task visibility ordering
    /// is required.
    pub dns_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub dial_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub tls_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl PhasedConnector {
    /// Build a connector with the given per-phase timeouts.
    pub fn new(timeouts: PhasedTimeouts) -> Self {
        let s = Self {
            dns_ms: std::sync::Arc::new(
                std::sync::atomic::AtomicU64::new(timeouts.dns.as_millis() as u64),
            ),
            dial_ms: std::sync::Arc::new(
                std::sync::atomic::AtomicU64::new(timeouts.dial.as_millis() as u64),
            ),
            tls_ms: std::sync::Arc::new(
                std::sync::atomic::AtomicU64::new(timeouts.tls.as_millis() as u64),
            ),
        };
        s
    }

    /// Build a connector with the system default timeouts (5s each).
    pub fn with_defaults() -> Self {
        Self::new(PhasedTimeouts::default())
    }

    /// Set the per-phase timeouts (in millis). Called by
    /// `call_inner` just before issuing the request. The connector
    /// reads these in its `call` method.
    pub fn set_timeouts(&self, timeouts: PhasedTimeouts) {
        use std::sync::atomic::Ordering;
        self.dns_ms.store(timeouts.dns.as_millis() as u64, Ordering::Relaxed);
        self.dial_ms.store(timeouts.dial.as_millis() as u64, Ordering::Relaxed);
        self.tls_ms.store(timeouts.tls.as_millis() as u64, Ordering::Relaxed);
    }

    /// Read the current per-phase timeouts (in millis).
    pub fn timeouts(&self) -> PhasedTimeouts {
        use std::sync::atomic::Ordering;
        let dns = std::time::Duration::from_millis(self.dns_ms.load(Ordering::Relaxed));
        let dial = std::time::Duration::from_millis(self.dial_ms.load(Ordering::Relaxed));
        let tls = std::time::Duration::from_millis(self.tls_ms.load(Ordering::Relaxed));
        PhasedTimeouts { dns, dial, tls }
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
        let timeouts = self.timeouts();
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

    // ---- Phase 1: DNS ---------------------------------------------------
    // If `host` is already a literal IP, skip DNS (and attribute any
    // later timeout to `Dial`, not `Dns`).
    let addrs: Vec<SocketAddr> = if let Some(literal) = parse_literal_ip(&host, port) {
        vec![literal]
    } else {
        match tokio::time::timeout(timeouts.dns, resolve_host(&host, port)).await {
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

    // ---- Phase 2: Dial --------------------------------------------------
    // Try the addresses in order. The first one that succeeds wins; if
    // all fail with I/O errors, return the last one. A `timeouts.dial`
    // expiry on any single attempt is reported as `Timeout(Dial)` so
    // a stuck connect is correctly attributed.
    let mut last_err: Option<io::Error> = None;
    let mut stream: Option<TcpStream> = None;
    for addr in addrs {
        match tokio::time::timeout(timeouts.dial, TcpStream::connect(addr)).await {
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
                kind: PhasedErrorKind::Io(last_err.unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::Other, "no addresses to dial")
                })),
            }));
        }
    };

    // Best-effort nodelay. Failure here is not fatal (a slow consumer
    // is hyper's problem, not ours).
    let _ = stream.set_nodelay(true);

    // ---- Phase 3: TLS ---------------------------------------------------
    // Gate 0: structural placeholder. The real implementation plugs
    // `tokio_rustls::client::TlsConnector::connect` in here, bounded
    // by `timeouts.tls`.
    if is_https {
        match tokio::time::timeout(timeouts.tls, tls_handshake(&mut stream)).await {
            Ok(Ok(())) => {}
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

    Ok(TokioIo::new(stream))
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
    let port = uri
        .port_u16()
        .unwrap_or(match uri.scheme_str() {
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
/// async DNS. We collect all addresses (not just the first) so the
/// dial phase can try each one in order; the dial phase itself has
/// its own timeout.
async fn resolve_host(host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    let lookup = format!("{}:{}", host, port);
    let addrs = tokio::net::lookup_host(lookup).await?;
    Ok(addrs.collect())
}

/// Gate-0 placeholder for the TLS handshake. The real implementation
/// will plug `tokio_rustls::client::TlsConnector::connect` here; for
/// now we just return success so the HTTP path keeps working and the
/// per-phase timeout machinery is testable end-to-end via the unit
/// tests.
async fn tls_handshake(_stream: &mut TcpStream) -> io::Result<()> {
    // TODO (gate 1+): plug tokio_rustls in here. The signature is
    // compatible: we receive a `&mut TcpStream` and return `io::Result<()>`.
    Ok(())
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
        if let Some(p) = e.downcast_ref::<PhasedConnectorError>() {
            return Some(p.phase);
        }
        current = e.source();
    }
    None
}

// Silence the unused-import warning for `Read`/`Write`/`HyperConnection`
// in builds where the `Service` impl is not exercised; they're needed
// to satisfy the `Connect` blanket impl's bounds. We assert at
// compile time that the `TokioIo<TcpStream>` response type is a valid
// hyper-util connect response.
#[allow(dead_code)]
fn _assert_impl_bounds() {
    fn _assert<R: Read + Write + HyperConnection + Unpin + Send + 'static>() {}
    _assert::<TokioIo<TcpStream>>();
}
