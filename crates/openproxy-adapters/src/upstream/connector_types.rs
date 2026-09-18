use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use hyper::rt::{Read, Write};
use hyper_util::client::legacy::connect::Connection as HyperConnection;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream as ClientTlsStream;

use super::phases::UpstreamPhase;

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
/// shared across every HTTPS request.
pub(crate) fn tls_connector() -> TlsConnector {
    static CONFIG: std::sync::LazyLock<Arc<rustls::ClientConfig>> =
        std::sync::LazyLock::new(|| {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let mut config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
            Arc::new(config)
        });
    TlsConnector::from(Arc::clone(&CONFIG))
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
                "phased connector: invalid URI in phase `{}`: {s}",
                self.phase
            ),
            PhasedErrorKind::Io(e) => write!(
                f,
                "phased connector: I/O error in phase `{}`: {e}",
                self.phase
            ),
        }
    }
}

impl std::error::Error for PhasedConnectorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            PhasedErrorKind::Io(e) => Some(e),
            PhasedErrorKind::Timeout | PhasedErrorKind::InvalidUri(_) => None,
        }
    }
}
