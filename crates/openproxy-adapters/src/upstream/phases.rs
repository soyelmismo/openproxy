//! Pipeline phases that an `UpstreamClient::call` advances through.
//!
//! Each phase is modelled explicitly so a timeout can be raced against the I/O
//! and attributed to one step, which hyper's single `connect_timeout` cannot do.

use std::fmt;
use std::time::{Duration, Instant};

/// A single step in the request pipeline.
///
/// The order is significant: phases advance in declaration order. The total
/// budget (`total_ms`) is an OUTERMOST ceiling, so a call that burns every
/// per-phase budget reports the phase it was waiting on at that instant,
/// typically `Headers` or `Body`. The total budget is enforced as a
/// `tokio::time::timeout` labeled `UpstreamPhase::Headers` for the dispatch
/// future and `UpstreamPhase::Body` for the body stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum UpstreamPhase {
    /// Resolving the hostname to one or more socket addresses.
    Dns = 0,
    /// Establishing the TCP connection to a resolved address.
    Dial = 1,
    /// Performing the TLS handshake (HTTPS only).
    Tls = 2,
    /// Writing the request line, headers, and (for non-streaming bodies)
    /// the full request body to the wire.
    Write = 3,
    /// Waiting for the response status line and headers from the server.
    Headers = 4,
    /// Reading the response body, chunk-by-chunk. Each chunk is bounded
    /// by `body_chunk_ms`; the total body is bounded by `total_ms`.
    Body = 5,
    /// `total_ms` fired while reading the body. Distinct from `Body`, which
    /// means the per-chunk `idle_chunk_ms` gap fired. The pipeline maps
    /// `Body` → `idle_chunk` and `Total` → `total` so the error names the timer
    /// that killed the request.
    Total = 6,
}

impl UpstreamPhase {
    /// Stable name used in tracing events and log lines.
    pub const fn as_str(&self) -> &'static str {
        const NAMES: [&str; 7] = ["dns", "dial", "tls", "write", "headers", "body", "total"];
        NAMES[*self as usize]
    }

    /// Corresponding configuration key in timeouts config.
    pub const fn config_hint(&self) -> &'static str {
        const HINTS: [&str; 7] = [
            "connect_ms",
            "connect_ms",
            "connect_ms",
            "request_send_ms",
            "ttft_ms",
            "idle_chunk_ms",
            "total_ms",
        ];
        HINTS[*self as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::profile::ResolvedTimeouts;

    #[test]
    fn test_upstream_phase_as_str() {
        assert_eq!(UpstreamPhase::Dns.as_str(), "dns");
        assert_eq!(UpstreamPhase::Dial.as_str(), "dial");
        assert_eq!(UpstreamPhase::Tls.as_str(), "tls");
        assert_eq!(UpstreamPhase::Write.as_str(), "write");
        assert_eq!(UpstreamPhase::Headers.as_str(), "headers");
        assert_eq!(UpstreamPhase::Body.as_str(), "body");
        assert_eq!(UpstreamPhase::Total.as_str(), "total");
    }

    #[test]
    fn test_upstream_phase_display() {
        assert_eq!(format!("{}", UpstreamPhase::Dns), "dns");
    }

    #[test]
    fn test_resolved_phase_deadlines() {
        let start = Instant::now();
        let timeouts = ResolvedTimeouts {
            dns_ms: 100,
            dial_ms: 200,
            tls_ms: 300,
            write_ms: 400,
            headers_ms: 500,
            body_chunk_ms: 600,
            total_ms: 700,
        };

        let deadlines = ResolvedPhaseDeadlines::from_profile(start, &timeouts);
        assert_eq!(deadlines.start, start);
        assert_eq!(deadlines.dns_deadline, start + Duration::from_millis(100));
        assert_eq!(deadlines.dial_deadline, start + Duration::from_millis(200));
        assert_eq!(deadlines.tls_deadline, start + Duration::from_millis(300));
        assert_eq!(deadlines.write_deadline, start + Duration::from_millis(400));
        assert_eq!(
            deadlines.headers_deadline,
            start + Duration::from_millis(500)
        );
        assert_eq!(
            deadlines.body_chunk_deadline,
            start + Duration::from_millis(600)
        );
        assert_eq!(deadlines.total_deadline, start + Duration::from_millis(700));

        assert_eq!(
            deadlines.deadline_for(UpstreamPhase::Dns),
            deadlines.dns_deadline
        );
        assert_eq!(
            deadlines.deadline_for(UpstreamPhase::Total),
            deadlines.total_deadline
        );
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
/// Which layer enforces each deadline:
///
/// - `dns_deadline`, `dial_deadline`, `tls_deadline` are enforced by
///   `PhasedConnector` (see `connector.rs`) with a real
///   `tokio::time::timeout` per phase. A stall surfaces as
///   `PhasedConnectorError { phase, Timeout }`, which the client recovers by
///   downcasting the boxed error. The fields stay on the struct so the full
///   deadline table is observable in one debug print.
/// - Upload and streaming TTFT are phase-relative, observed in `client.rs`.
///   The corresponding fields here are reference instants for diagnostics
///   and test-connector phase hints, not races over the entire dispatch.
/// - Unary generation uses only `total_deadline`, before and after headers.
/// - The body stream resets its idle timer only after meaningful stream
///   content, and keeps the same post-upload TTFT deadline before that.
/// - `total_deadline` is an absolute ceiling across all phases.
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
        let deadlines = [
            self.dns_deadline,
            self.dial_deadline,
            self.tls_deadline,
            self.write_deadline,
            self.headers_deadline,
            self.body_chunk_deadline,
            self.total_deadline,
        ];
        deadlines[phase as usize]
    }
}
