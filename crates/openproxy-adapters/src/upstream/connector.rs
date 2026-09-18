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
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use http::Uri;
use hyper::rt::{Read, Write};
use hyper_util::client::legacy::connect::Connection as HyperConnection;
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tower_service::Service;

pub use super::connector_types::*;
pub use super::dns::is_private_or_reserved;
use super::dns::resolve_host;
use super::phases::UpstreamPhase;
use super::proxy_tunnel::{ProxyConfig, parse_proxy_url, run_proxy_tunnel};

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

fn resolve_call_proxy_config()
-> Result<Option<ProxyConfig>, Box<dyn std::error::Error + Send + Sync>> {
    let proxy_opt = CALL_PROXY
        .try_with(std::clone::Clone::clone)
        .unwrap_or(None);
    if let Some(ref proxy_url) = proxy_opt {
        parse_proxy_url(proxy_url).map(Some).map_err(|e| {
            Box::new(PhasedConnectorError {
                phase: UpstreamPhase::Dns,
                kind: PhasedErrorKind::InvalidUri(format!("Invalid proxy config: {e}")),
            }) as Box<dyn std::error::Error + Send + Sync>
        })
    } else {
        Ok(None)
    }
}

async fn dns_phase(
    dial_host: &str,
    dial_port: u16,
    connect_deadline: std::time::Instant,
    dns_timeout_config: Duration,
) -> Result<Vec<SocketAddr>, PhasedConnectorError> {
    if let Some(literal) = parse_literal_ip(dial_host, dial_port) {
        return Ok(vec![literal]);
    }
    let dns_remaining = connect_deadline
        .checked_duration_since(std::time::Instant::now())
        .unwrap_or(Duration::from_millis(0));
    let dns_timeout = dns_timeout_config.min(dns_remaining);
    if dns_timeout.is_zero() {
        return Err(PhasedConnectorError {
            phase: UpstreamPhase::Dns,
            kind: PhasedErrorKind::Timeout,
        });
    }
    match tokio::time::timeout(dns_timeout, resolve_host(dial_host, dial_port)).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(PhasedConnectorError {
            phase: UpstreamPhase::Dns,
            kind: PhasedErrorKind::Io(e),
        }),
        Err(_) => Err(PhasedConnectorError {
            phase: UpstreamPhase::Dns,
            kind: PhasedErrorKind::Timeout,
        }),
    }
}

fn filter_ssrf_addresses(addrs: Vec<SocketAddr>) -> Result<Vec<SocketAddr>, PhasedConnectorError> {
    let allow_private = cfg!(test)
        || cfg!(feature = "ssrf-bypass")
        || std::env::var("OPENPROXY_ALLOW_PRIVATE_UPSTREAMS")
            .is_ok_and(|v| v == "true" || v == "1");

    if allow_private {
        if cfg!(test) {
            tracing::debug!("SSRF: filter disabled in test build");
        } else {
            tracing::debug!(
                "SSRF: private upstream connections allowed via OPENPROXY_ALLOW_PRIVATE_UPSTREAMS"
            );
        }
        return Ok(addrs);
    }

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
        return Err(PhasedConnectorError {
            phase: UpstreamPhase::Dial,
            kind: PhasedErrorKind::Io(io::Error::other(
                "all resolved addresses are private/reserved (SSRF block). Set OPENPROXY_ALLOW_PRIVATE_UPSTREAMS=true to allow.",
            )),
        });
    }
    Ok(filtered)
}

pub(crate) async fn dial_phase(
    addrs: Vec<SocketAddr>,
    connect_deadline: std::time::Instant,
    dial_timeout_config: Duration,
) -> Result<TcpStream, PhasedConnectorError> {
    if addrs.is_empty() {
        return Err(PhasedConnectorError {
            phase: UpstreamPhase::Dial,
            kind: PhasedErrorKind::Io(io::Error::other("no addresses to dial")),
        });
    }

    let mut last_err: Option<io::Error> = None;
    let mut saw_timeout = false;
    let total = addrs.len();

    for (idx, addr) in addrs.into_iter().enumerate() {
        let dial_remaining = connect_deadline
            .checked_duration_since(std::time::Instant::now())
            .unwrap_or(Duration::ZERO);
        if dial_remaining.is_zero() {
            saw_timeout = true;
            break;
        }

        let remaining = (total - idx) as u32;
        let per_limit = if remaining > 1 {
            dial_timeout_config
                .min(dial_remaining)
                .min((dial_remaining / remaining).max(Duration::from_millis(1500)))
        } else {
            dial_timeout_config.min(dial_remaining)
        };
        if per_limit.is_zero() {
            saw_timeout = true;
            break;
        }

        match tokio::time::timeout(per_limit, TcpStream::connect(addr)).await {
            Ok(Ok(s)) => return Ok(s),
            Ok(Err(e)) => {
                tracing::debug!(addr = %addr, error = %e, "dial attempt failed; trying next IP");
                last_err = Some(e);
            }
            Err(_) => {
                tracing::warn!(addr = %addr, "dial attempt timed out; trying next IP");
                saw_timeout = true;
                last_err = Some(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("dial timed out for {addr}"),
                ));
            }
        }
    }

    if saw_timeout
        && last_err
            .as_ref()
            .is_some_and(|e| e.kind() == io::ErrorKind::TimedOut)
    {
        Err(PhasedConnectorError {
            phase: UpstreamPhase::Dial,
            kind: PhasedErrorKind::Timeout,
        })
    } else {
        Err(PhasedConnectorError {
            phase: UpstreamPhase::Dial,
            kind: PhasedErrorKind::Io(
                last_err.unwrap_or_else(|| io::Error::other("all addresses failed to dial")),
            ),
        })
    }
}

async fn proxy_tunnel_phase(
    stream: TcpStream,
    proxy_config_opt: Option<&ProxyConfig>,
    host: &str,
    port: u16,
    connect_deadline: std::time::Instant,
) -> Result<TcpStream, PhasedConnectorError> {
    let Some(proxy_config) = proxy_config_opt else {
        return Ok(stream);
    };

    let dial_remaining = connect_deadline
        .checked_duration_since(std::time::Instant::now())
        .unwrap_or(Duration::from_millis(0));
    if dial_remaining.is_zero() {
        return Err(PhasedConnectorError {
            phase: UpstreamPhase::Dial,
            kind: PhasedErrorKind::Timeout,
        });
    }

    match tokio::time::timeout(
        dial_remaining,
        run_proxy_tunnel(stream, proxy_config, host, port),
    )
    .await
    {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => Err(PhasedConnectorError {
            phase: UpstreamPhase::Dial,
            kind: PhasedErrorKind::Io(io::Error::other(format!("Proxy handshake failed: {e}"))),
        }),
        Err(_) => Err(PhasedConnectorError {
            phase: UpstreamPhase::Dial,
            kind: PhasedErrorKind::Timeout,
        }),
    }
}

fn configure_tcp_stream(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);
    let sock = socket2::SockRef::from(stream);
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(30))
        .with_interval(Duration::from_secs(10));
    let _ = sock.set_tcp_keepalive(&keepalive);
}

async fn tls_phase(
    stream: TcpStream,
    host: &str,
    connect_deadline: std::time::Instant,
    tls_timeout_config: Duration,
) -> Result<PhasedConnection, PhasedConnectorError> {
    let server_name = ServerName::try_from(host.to_string()).map_err(|e| PhasedConnectorError {
        phase: UpstreamPhase::Tls,
        kind: PhasedErrorKind::InvalidUri(format!("bad SNI host: {e}")),
    })?;
    let connector = tls_connector();
    let tls_remaining = connect_deadline
        .checked_duration_since(std::time::Instant::now())
        .unwrap_or(Duration::from_millis(0));
    let tls_timeout = tls_timeout_config.min(tls_remaining);
    if tls_timeout.is_zero() {
        return Err(PhasedConnectorError {
            phase: UpstreamPhase::Tls,
            kind: PhasedErrorKind::Timeout,
        });
    }
    match tokio::time::timeout(tls_timeout, connector.connect(server_name, stream)).await {
        Ok(Ok(tls_stream)) => {
            let (_, client_conn) = tls_stream.get_ref();
            let negotiated_h2 = client_conn.alpn_protocol().is_some_and(|p| p == b"h2");
            Ok(PhasedConnection::Tls {
                io: Box::new(TokioIo::new(tls_stream)),
                negotiated_h2,
            })
        }
        Ok(Err(e)) => Err(PhasedConnectorError {
            phase: UpstreamPhase::Tls,
            kind: PhasedErrorKind::Io(e),
        }),
        Err(_) => Err(PhasedConnectorError {
            phase: UpstreamPhase::Tls,
            kind: PhasedErrorKind::Timeout,
        }),
    }
}

fn resolve_dial_target<'a>(
    proxy: Option<&'a ProxyConfig>,
    host: &'a str,
    port: u16,
) -> (&'a str, u16) {
    proxy.map_or((host, port), |p| (p.host.as_str(), p.port))
}

async fn establish_raw_tcp_stream(
    dial_host: &str,
    dial_port: u16,
    connect_deadline: std::time::Instant,
    timeouts: PhasedTimeouts,
) -> Result<TcpStream, Box<dyn std::error::Error + Send + Sync>> {
    let addrs = dns_phase(dial_host, dial_port, connect_deadline, timeouts.dns).await?;
    let filtered_addrs = filter_ssrf_addresses(addrs)?;
    Ok(dial_phase(filtered_addrs, connect_deadline, timeouts.dial).await?)
}

/// The actual connect future. Pulled out as a free function so the
/// `Service::call` signature stays simple.
async fn run_phased_connect(
    uri: Uri,
    is_https: bool,
    timeouts: PhasedTimeouts,
) -> Result<PhasedConnection, Box<dyn std::error::Error + Send + Sync>> {
    let connect_start = std::time::Instant::now();
    let connect_budget = timeouts.dns.max(timeouts.dial).max(timeouts.tls);
    let connect_deadline = connect_start + connect_budget;

    let (host, port) = parse_authority(&uri).map_err(|msg| {
        Box::new(PhasedConnectorError {
            phase: UpstreamPhase::Dns,
            kind: PhasedErrorKind::InvalidUri(msg),
        })
    })?;

    let proxy_config_opt = resolve_call_proxy_config()?;
    let (dial_host, dial_port) = resolve_dial_target(proxy_config_opt.as_ref(), host, port);

    let stream = establish_raw_tcp_stream(dial_host, dial_port, connect_deadline, timeouts).await?;
    let stream = proxy_tunnel_phase(
        stream,
        proxy_config_opt.as_ref(),
        host,
        port,
        connect_deadline,
    )
    .await?;
    configure_tcp_stream(&stream);

    if is_https {
        tls_phase(stream, host, connect_deadline, timeouts.tls)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    } else {
        Ok(PhasedConnection::Plain(TokioIo::new(stream)))
    }
}

/// `host:port` -> (host, port) with sensible defaults. Returns an
/// error string (not a `PhasedConnectorError`) so the caller can wrap
/// it with the right phase.
fn parse_authority(uri: &Uri) -> Result<(&str, u16), String> {
    let host = uri
        .host()
        .ok_or_else(|| "missing host".to_string())?
        .trim_start_matches('[')
        .trim_end_matches(']');
    let port = uri.port_u16().unwrap_or(match uri.scheme_str() {
        Some("https") => 443,
        _ => 80,
    });
    Ok((host, port))
}

/// If `host` is an IP literal (v4 or v6), build the corresponding
/// `SocketAddr` directly so we can skip the DNS step.
fn parse_literal_ip(host: &str, port: u16) -> Option<SocketAddr> {
    host.parse::<IpAddr>()
        .ok()
        .map(|ip| SocketAddr::new(ip, port))
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

#[cfg(test)]
#[path = "connector_tests.rs"]
mod tests;
