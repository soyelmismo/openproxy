//! Error types for the `UpstreamClient`.
//!
//! `UpstreamError` is the single error surface that call sites see.
//! Each variant is non-exhaustive inside (`#[non_exhaustive]`) so future
//! gates can add context without a breaking change.

use super::phases::UpstreamPhase;
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
            UpstreamError::Timeout(p) => write!(f, "upstream timeout in phase `{p}`"),
            UpstreamError::Connection(m) => write!(f, "upstream connection error: {m}"),
            UpstreamError::Tls(m) => write!(f, "upstream TLS error: {m}"),
            UpstreamError::Cancel => f.write_str("upstream call cancelled"),
            UpstreamError::Http(m) => write!(f, "upstream HTTP error: {m}"),
            UpstreamError::Decode(m) => write!(f, "upstream decode error: {m}"),
            UpstreamError::Invalid(m) => write!(f, "upstream invalid request: {m}"),
        }
    }
}

impl std::error::Error for UpstreamError {}

impl UpstreamError {
    /// Convert an `UpstreamError` into a `CoreError`, optionally prefixing
    /// connection/protocol errors with contextual details (e.g. endpoint name or URL).
    pub fn to_core_error(&self, context: &str) -> openproxy_types::CoreError {
        match self {
            UpstreamError::Cancel => openproxy_types::CoreError::Cancelled(
                openproxy_types::CancelReason::ClientDisconnected,
            ),
            UpstreamError::Timeout(phase) => openproxy_types::CoreError::UpstreamTimeout {
                phase: phase.to_string(),
                ms: 0,
            },
            UpstreamError::Connection(_)
            | UpstreamError::Tls(_)
            | UpstreamError::Http(_)
            | UpstreamError::Decode(_)
            | UpstreamError::Invalid(_) => {
                let msg = if context.is_empty() {
                    format!("{self}")
                } else {
                    format!("{context}: {self}")
                };
                openproxy_types::CoreError::UpstreamConnection(msg)
            }
        }
    }
}

impl From<UpstreamError> for openproxy_types::CoreError {
    fn from(err: UpstreamError) -> Self {
        err.to_core_error("")
    }
}

/// Convenience result alias.
pub type UpstreamResult<T> = std::result::Result<T, UpstreamError>;
