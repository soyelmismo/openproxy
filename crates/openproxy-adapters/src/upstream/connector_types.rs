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

/// Transport returned by the proxy-tunnel phase (OP-28).
///
/// `Plain` is a raw TCP stream (direct connection, or an http/socks proxy
/// tunnel). `TlsToProxy` is a TLS session established with an `https://`
/// proxy, inside which the CONNECT tunnel runs — the CONNECT request and its
/// `Proxy-Authorization: Basic` header must not travel in cleartext.
pub enum MaybeTlsStream {
    Plain(TcpStream),
    TlsToProxy(Box<ClientTlsStream<TcpStream>>),
}

impl tokio::io::AsyncRead for MaybeTlsStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<Result<(), io::Error>> {
        match &mut *self {
            MaybeTlsStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            MaybeTlsStream::TlsToProxy(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for MaybeTlsStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        match &mut *self {
            MaybeTlsStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            MaybeTlsStream::TlsToProxy(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match &mut *self {
            MaybeTlsStream::Plain(s) => Pin::new(s).poll_flush(cx),
            MaybeTlsStream::TlsToProxy(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match &mut *self {
            MaybeTlsStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            MaybeTlsStream::TlsToProxy(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

pub enum PhasedConnection {
    Plain(Box<TokioIo<MaybeTlsStream>>),
    /// `true` when ALPN negotiated `h2`. `connected()` hands this to
    /// hyper-util to pick the HTTP/2 or HTTP/1.1 parser; a wrong answer surfaces
    /// as `invalid HTTP version parsed` once the request reaches the body.
    Tls {
        io: Box<TokioIo<ClientTlsStream<MaybeTlsStream>>>,
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
            PhasedConnection::Plain(io) => Pin::new(&mut **io).poll_read(cx, buf),
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
            PhasedConnection::Plain(io) => Pin::new(&mut **io).poll_write(cx, buf),
            PhasedConnection::Tls { io, .. } => Pin::new(&mut **io).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match &mut *self {
            PhasedConnection::Plain(io) => Pin::new(&mut **io).poll_flush(cx),
            PhasedConnection::Tls { io, .. } => Pin::new(&mut **io).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        match &mut *self {
            PhasedConnection::Plain(io) => Pin::new(&mut **io).poll_shutdown(cx),
            PhasedConnection::Tls { io, .. } => Pin::new(&mut **io).poll_shutdown(cx),
        }
    }
}

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

/// Process-wide `TlsConnector` with webpki roots. `ClientConfig` is an `Arc`
/// internally, so the clone per request is cheap.
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

/// Per-phase timeouts carried by a `PhasedConnector`. Each value is a max
/// duration enforced with `tokio::time::timeout`, which reports the stalled
/// phase on expiry.
#[derive(Debug, Clone, Copy)]
pub struct PhasedTimeouts {
    pub dns: Duration,
    pub dial: Duration,
    pub tls: Duration,
}

impl PhasedTimeouts {
    pub fn from_resolved(t: &super::profile::ResolvedTimeouts) -> Self {
        Self {
            dns: Duration::from_millis(t.dns_ms),
            dial: Duration::from_millis(t.dial_ms),
            tls: Duration::from_millis(t.tls_ms),
        }
    }
}

impl Default for PhasedTimeouts {
    /// 5s per phase, matching `SYSTEM_DEFAULTS` in `profile.rs`.
    fn default() -> Self {
        Self {
            dns: Duration::from_secs(5),
            dial: Duration::from_secs(5),
            tls: Duration::from_secs(5),
        }
    }
}

/// Errors from `PhasedConnector::call`. Carries a `phase` so `client::call_inner`
/// can attribute a timeout to the right step; downcast
/// `Box<dyn Error + Send + Sync>` to recover it.
#[derive(Debug)]
pub struct PhasedConnectorError {
    pub phase: UpstreamPhase,
    pub kind: PhasedErrorKind,
}

#[derive(Debug)]
pub enum PhasedErrorKind {
    Timeout,
    InvalidUri(String),
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
