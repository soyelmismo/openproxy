//! Pipeline phases that an `UpstreamClient::call` advances through.
//!
//! Each phase is modelled explicitly so the caller (or a unit test) can
//! race the per-phase timeout against the I/O and attribute a timeout
//! to a specific phase. This is the central piece of the migration off
//! reqwest (which only exposes a single `connect_timeout`).

use std::fmt;
use std::time::{Duration, Instant};

/// A single step in the request pipeline.
///
/// The order is significant: phases are advanced in declaration order
/// (DNS, Dial, Tls, Write, Headers, Body). The total budget
/// (`total_ms`) is an OUTERMOST ceiling: when the call has burned
/// through every per-phase budget, the timeout is reported as the
/// phase whose budget was being waited on at that instant (typically
/// `Headers` or `Body`). The total budget is the absolute hard cap
/// and is enforced as a `tokio::time::timeout` whose label is
/// `UpstreamPhase::Headers` for the dispatch future and
/// `UpstreamPhase::Body` for the body stream. This was the existing
/// behavior of the soft-accumulation version, kept verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UpstreamPhase {
    /// Resolving the hostname to one or more socket addresses.
    Dns,
    /// Establishing the TCP connection to a resolved address.
    Dial,
    /// Performing the TLS handshake (HTTPS only).
    Tls,
    /// Writing the request line, headers, and (for non-streaming bodies)
    /// the full request body to the wire.
    Write,
    /// Waiting for the response status line and headers from the server.
    Headers,
    /// Reading the response body, chunk-by-chunk. Each chunk is bounded
    /// by `body_chunk_ms`; the total body is bounded by `total_ms`.
    Body,
}

impl UpstreamPhase {
    /// Stable name used in tracing events and log lines.
    pub fn as_str(&self) -> &'static str {
        match self {
            UpstreamPhase::Dns => "dns",
            UpstreamPhase::Dial => "dial",
            UpstreamPhase::Tls => "tls",
            UpstreamPhase::Write => "write",
            UpstreamPhase::Headers => "headers",
            UpstreamPhase::Body => "body",
        }
    }
}

impl fmt::Display for UpstreamPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Absolute deadlines (vs. `start`) for each phase plus the call total.
///
/// Built once per `UpstreamClient::call` from the resolved `TimeoutProfile`.
/// Each phase races its I/O future against a `sleep_until` for its own
/// deadline; `total_deadline` is checked at every step.
///
/// # Structural note: which deadlines are actually enforced?
///
/// Bug 2b/2c fix: as of this revision, EACH phase has its own real
/// enforcement — no more `min(headers, write, dial, tls, total)`
/// soft-accumulation. The breakdown is:
///
/// - `dns_deadline` is **enforced** by the `PhasedConnector` (see
///   `connector.rs`) with a real `tokio::time::timeout` around
///   `tokio::net::lookup_host`. A stalled DNS lookup surfaces as
///   `PhasedConnectorError { phase: Dns, Timeout }` and the client
///   downcasts the boxed error to attribute the timeout correctly.
///   This field stays on the struct for completeness and so the
///   full deadline table is observable in a single debug print.
///
/// - `dial_deadline` and `tls_deadline` are **enforced** by the
///   `PhasedConnector` the same way. A stalled TCP connect surfaces
///   as `PhasedConnectorError { phase: Dial, Timeout }`; a stalled
///   TLS handshake surfaces as `PhasedConnectorError { phase: Tls,
///   Timeout }`.
///
/// - `write_deadline` is **enforced** by the OUTER nested
///   `tokio::time::timeout` in `UpstreamClient::call_inner`. The
///   `legacy::Client::request` future is wrapped in a race against
///   `write_deadline` and the timeout is attributed to `Write`.
///   (The body upload happens before hyper can start reading the
///   response, so the write phase is naturally bounded by this
///   outer race — the previous "soft-accumulation" version credited
///   the timeout to `Headers` instead, which violated the contract.)
///
/// - `headers_deadline` is **enforced** by the INNER nested
///   `tokio::time::timeout` in `UpstreamClient::call_inner`. It only
///   fires if the dispatch future is still in flight AFTER
///   `write_deadline` resolved (i.e. the body uploaded on time and
///   the server is now slow to respond). A `Timeout(Headers)` from
///   this inner race is the canonical "server is slow" attribution.
///
/// - `body_chunk_deadline` is **NOT** a deadline relative to `start`;
///   it lives in the body stream and is recomputed inside
///   `UpstreamBodyStream::next_chunk` as `last_chunk_at + body_chunk_ms`.
///   The instant stored in this field is therefore used as the
///   "implicit TTFT anchor" for the first chunk only; subsequent
///   chunks honor the gap. The field is kept on the struct so the
///   total budget and a debug-print of all deadlines stay consistent
///   with the `ResolvedTimeouts` they came from.
///
/// - `total_deadline` is the OUTERMOST nested
///   `tokio::time::timeout` in `UpstreamClient::call_inner`. It is
///   the absolute ceiling; a stalled call that has burned through
///   every per-phase budget surfaces as `Timeout(Headers)` (the
///   closest existing phase boundary the dispatch future was
///   waiting on — `UpstreamPhase` does not have a `Total` variant).
#[derive(Debug, Clone, Copy)]
pub struct ResolvedPhaseDeadlines {
    pub start: Instant,
    pub dns_deadline: Instant,
    pub dial_deadline: Instant,
    pub tls_deadline: Instant,
    pub write_deadline: Instant,
    pub headers_deadline: Instant,
    pub body_chunk_deadline: Instant,
    pub total_deadline: Instant,
}

impl ResolvedPhaseDeadlines {
    /// Build from a `start` instant and a `ResolvedTimeouts` profile.
    pub fn from_profile(start: Instant, t: &super::profile::ResolvedTimeouts) -> Self {
        Self {
            start,
            dns_deadline: start + Duration::from_millis(t.dns_ms),
            dial_deadline: start + Duration::from_millis(t.dial_ms),
            tls_deadline: start + Duration::from_millis(t.tls_ms),
            write_deadline: start + Duration::from_millis(t.write_ms),
            headers_deadline: start + Duration::from_millis(t.headers_ms),
            body_chunk_deadline: start + Duration::from_millis(t.body_chunk_ms),
            total_deadline: start + Duration::from_millis(t.total_ms),
        }
    }

    /// The deadline for a given phase. `UpstreamPhase::Body` is special-cased
    /// to the per-chunk deadline; the total ceiling is checked separately.
    pub fn deadline_for(&self, phase: UpstreamPhase) -> Instant {
        match phase {
            UpstreamPhase::Dns => self.dns_deadline,
            UpstreamPhase::Dial => self.dial_deadline,
            UpstreamPhase::Tls => self.tls_deadline,
            UpstreamPhase::Write => self.write_deadline,
            UpstreamPhase::Headers => self.headers_deadline,
            UpstreamPhase::Body => self.body_chunk_deadline,
        }
    }
}
