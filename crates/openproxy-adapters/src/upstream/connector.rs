//! Real per-phase connector for the `upstream/` client.
//!
//! `hyper_util::client::legacy::Client::request` is a single future that
//! collapses DNS, dial, TLS, write and wait-for-headers into one. Timing that
//! one future with `min(headers_ms, write_ms, dial_ms, tls_ms, total_ms)`
//! ("soft-accumulation") can never emit `Timeout(Write)`: hyper does not say
//! where the body upload stopped, so a `write_ms = 200ms` cap on the upload
//! goes unreported. This module enforces the phases for real:
//!
//! - **DNS**, **Dial** and **TLS** are bounded inside the connector by
//!   independent `tokio::time::timeout` calls. A stall reports
//!   `Timeout(Dns|Dial|Tls)` through a `PhasedConnectorError` downcast on the
//!   boxed error.
//! - **Write** vs **Headers** are split by a nested `tokio::time::timeout` in
//!   `client::call_inner`: the outer race carries `write_ms` and reports
//!   `Timeout(Write)`, the inner one `headers_ms` and reports
//!   `Timeout(Headers)`. Whichever ceiling fires first wins.
//! - **Total** is the outermost ceiling.
//!
//! TLS: the HTTPS path upgrades the `TcpStream` via
//! `tokio_rustls::TlsConnector::connect`, bounded by `timeouts.tls`. This
//! module is the only place TLS is configured. `PhasedConnection` holds
//! either shape; both satisfy hyper-util's `Connect` blanket impl
//! (`Read + Write + Connection + Unpin + Send`).
//!
//! A wrapper around the hyper `Service::call` future was rejected: hyper-util
//! exposes no progress events for its internal `Service::call`, so any wrapper
//! degrades to a single timeout over the whole future. A custom
//! `tower_service::Service<Uri>` is the only way to get per-step deadlines.

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
/// `call(uri)` resolves the hostname (DNS, bounded by `timeouts.dns`), dials
/// the first address that answers (Dial, `timeouts.dial`) and, for `https`,
/// wraps the stream in a TLS handshake (Tls, `timeouts.tls`). Each phase is a
/// separate `tokio::time::timeout`; on expiry the future resolves to
/// `Err(PhasedConnectorError { phase, Timeout })`.
///
/// `hyper-util`'s `HttpConnector` collapses all three into one future, which
/// is why the per-phase attribution needs a custom connector. Only
/// `hyper_util::rt::TokioIo` is reused from it, as the `Read + Write +
/// Connection` wrapper the `Connect` blanket impl requires.
#[derive(Clone)]
pub struct PhasedConnector {
    /// Fallback for tests that build a `PhasedConnector` directly. Production
    /// paths set the `CALL_TIMEOUTS` task-local via `UpstreamClient::call_inner`.
    defaults: PhasedTimeouts,
}

// Per-call timeout injection.
//
// hyper-util clones the connector per request, so the connector is a `Clone`
// value shared across concurrent calls with no per-call setup hook, and the
// deadlines cannot travel as a `call()` argument.
//
// Storing them in `Arc<AtomicU64>` fields was race-prone: the caller wrote the
// timeouts before polling the dispatch future, and `tokio::select!` does not
// poll that future synchronously. Between the write and the first poll another
// request's `call_inner` could clobber the atomics and inherit them.
//
// A `tokio::task_local!` slot gives each task its own copy: the caller wraps
// the dispatch future in `CALL_TIMEOUTS.scope(value, future)` and the
// connector reads it via `try_with`, falling back to `defaults` when unset.
tokio::task_local! {
    pub(crate) static CALL_TIMEOUTS: PhasedTimeouts;
}
tokio::task_local! {
    pub static CALL_PROXY: Option<String>;
}

impl PhasedConnector {
    /// Build a connector with the given per-phase timeouts, used as the
    /// fallback when the `CALL_TIMEOUTS` task-local is unset.
    pub fn new(timeouts: PhasedTimeouts) -> Self {
        Self { defaults: timeouts }
    }

    /// Build a connector with the system default timeouts.
    pub fn with_defaults() -> Self {
        Self::new(PhasedTimeouts::default())
    }

    /// Read the effective per-phase timeouts: the `CALL_TIMEOUTS` task-local
    /// first, then the stored `defaults` when the slot is unset.
    pub fn effective_timeouts(&self) -> PhasedTimeouts {
        CALL_TIMEOUTS.try_with(|t| *t).unwrap_or(self.defaults)
    }

    /// Source-compat no-op kept for tests that call `set_timeouts` directly.
    /// Per-call timeouts arrive through the `CALL_TIMEOUTS` task-local.
    /// `defaults` is not mutated: the connector is shared across concurrent
    /// requests via `Clone`, so writing it would restore the race.
    pub fn set_timeouts(&self, _timeouts: PhasedTimeouts) {
        // No-op: per-call timeouts travel in the `CALL_TIMEOUTS` task-local.
    }

    /// The fallback timeouts, not the per-call task-local. Used by the
    /// `Debug` impl and by tests; production reads `effective_timeouts()`.
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
    // `S::Future: Unpin + Send`. A `Pin<Box<dyn Future + Send>>` is `Unpin`
    // for any inner type, so the async block below can await non-`Unpin`
    // futures (`tokio::time::Timeout`, `TcpStream::connect`) without itself
    // being `Unpin`.
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Always ready: the per-call future shares no state with `poll_ready`
        // (no rate limit, resolver pool or connect semaphore), matching
        // `HttpConnector` with `GaiResolver`.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        // Read the per-call timeouts from the task-local set by
        // `UpstreamClient::call_inner`; fall back to `defaults` when unset.
        let timeouts = self.effective_timeouts();
        let is_https = uri.scheme_str() == Some("https");
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
) -> Result<MaybeTlsStream, PhasedConnectorError> {
    let Some(proxy_config) = proxy_config_opt else {
        return Ok(MaybeTlsStream::Plain(stream));
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
    stream: MaybeTlsStream,
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

/// The connect future, pulled out so the `Service::call` signature stays
/// simple.
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
    // TCP tuning applies to the raw socket before any proxy/TLS wrapping.
    configure_tcp_stream(&stream);
    let stream = proxy_tunnel_phase(
        stream,
        proxy_config_opt.as_ref(),
        host,
        port,
        connect_deadline,
    )
    .await?;

    if is_https {
        tls_phase(stream, host, connect_deadline, timeouts.tls)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    } else {
        Ok(PhasedConnection::Plain(Box::new(TokioIo::new(stream))))
    }
}

/// `host:port` -> (host, port), defaulting the port from the scheme. Returns a
/// plain `String` error so the caller can attach the phase.
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

/// If `err` (or anything in its `source` chain) is a timed-out
/// `PhasedConnectorError`, return its phase. `None` means the caller falls
/// back to a different attribution (e.g. the `Headers` default).
pub fn phased_phase(err: &(dyn std::error::Error + 'static)) -> Option<UpstreamPhase> {
    // Walk the source chain so wrapped errors (e.g. a hyper-util `Connect`
    // wrapping our boxed error) are detected too.
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

// Compile-time pin: `PhasedConnection` and `TokioIo<TcpStream>` must satisfy
// the hyper-util `Connect` blanket impl's `Read + Write + Connection + Unpin +
// Send + 'static` bounds. The anonymous const block references `_assert` so
// the bound checks fire without a dead_code warning.
const _: () = {
    fn _assert<R: Read + Write + HyperConnection + Unpin + Send + 'static>() {}
    let _ = _assert::<PhasedConnection>;
    let _ = _assert::<TokioIo<TcpStream>>;
};

#[cfg(test)]
#[path = "connector_tests.rs"]
mod tests;
