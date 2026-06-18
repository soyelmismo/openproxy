//! Error types for the `UpstreamClient`.
//!
//! `UpstreamError` is the single error surface that call sites see.
//! Each variant is non-exhaustive inside (`#[non_exhaustive]`) so future
//! gates can add context without a breaking change.

use super::phases::UpstreamPhase;
use crate::error::CoreError;
use std::fmt;

/// Errors returned by `UpstreamClient::call`.
#[derive(Debug)]
#[non_exhaustive]
pub enum UpstreamError {
    /// A specific phase exceeded its deadline. The carried phase tells
    /// the caller (and tests) exactly which step stalled.
    Timeout(UpstreamPhase),
    /// TCP / DNS / IO failure while establishing the connection.
    Connection(String),
    /// TLS handshake failure (cert, protocol, ALPN).
    Tls(String),
    /// The call was cancelled via the `CancellationToken`.
    Cancel,
    /// A non-timeout HTTP-level error from the server side: status line
    /// malformed, response headers invalid, etc.
    Http(String),
    /// Failed to decode the response body (zstd, gzip, JSON, etc).
    Decode(String),
    /// Caller misuse: malformed request, invalid URL, etc.
    Invalid(String),
}

impl fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UpstreamError::Timeout(p) => write!(f, "upstream timeout in phase `{}`", p),
            UpstreamError::Connection(m) => write!(f, "upstream connection error: {}", m),
            UpstreamError::Tls(m) => write!(f, "upstream TLS error: {}", m),
            UpstreamError::Cancel => f.write_str("upstream call cancelled"),
            UpstreamError::Http(m) => write!(f, "upstream HTTP error: {}", m),
            UpstreamError::Decode(m) => write!(f, "upstream decode error: {}", m),
            UpstreamError::Invalid(m) => write!(f, "upstream invalid request: {}", m),
        }
    }
}

impl std::error::Error for UpstreamError {}

/// Convenience result alias.
pub type UpstreamResult<T> = std::result::Result<T, UpstreamError>;

/// Map an [`UpstreamError`] to the public [`CoreError`] used by the
/// pipeline. Only the *dispatch-time* clusters are covered: the
/// `Connection | Tls | Http | Decode | Invalid` variants all surface as
/// `CoreError::UpstreamConnection(msg)` because the dispatch error
/// paths in `dispatch_upstream` / `dispatch_upstream_streaming` do not
/// have an enclosing read prefix (the per-chunk streaming path uses a
/// `"stream read: …"` prefix and therefore keeps its own match).
///
/// `Timeout`, `Cancel`, and the HTTP status-code `UpstreamError`
/// path are NOT covered here because they need contextual info
/// (elapsed ms, the model name, etc.) that is only available at the
/// call site.
impl From<UpstreamError> for CoreError {
    fn from(e: UpstreamError) -> Self {
        match e {
            UpstreamError::Connection(msg)
            | UpstreamError::Tls(msg)
            | UpstreamError::Http(msg)
            | UpstreamError::Decode(msg)
            | UpstreamError::Invalid(msg) => CoreError::UpstreamConnection(msg),
            // The other variants require context the pipeline must
            // supply; constructing them here would lose the elapsed
            // ms and provider identity that the wire-level logs carry.
            UpstreamError::Timeout(_) | UpstreamError::Cancel => {
                unreachable!(
                    "UpstreamError::Timeout/Cancel must be mapped at the call site \
                     with elapsed_ms context; From is for the no-context clusters only"
                )
            }
        }
    }
}

/// Stable label used by the dispatch error paths when they report a
/// timeout to the dashboard. The legacy `reqwest`-era code used
/// `"connect"` for everything up to and including response headers
/// (because the legacy `tokio::time::timeout(connect, …)` covered the
/// dial + TLS + wait-for-headers wall-clock budget) and `"total"`
/// for body-phase stalls. The new phase model distinguishes them, so
/// collapse the pre-headers phases onto `"connect"` and the body
/// phase onto `"total"` to keep the dashboard's strings stable.
///
/// The streaming per-chunk path uses a different label
/// (`"idle_chunk"` for the per-chunk gap budget) and therefore does
/// not call this helper.
pub fn phase_label(phase: UpstreamPhase) -> &'static str {
    match phase {
        UpstreamPhase::Dns
        | UpstreamPhase::Dial
        | UpstreamPhase::Tls
        | UpstreamPhase::Write
        | UpstreamPhase::Headers => "connect",
        UpstreamPhase::Body => "total",
    }
}
