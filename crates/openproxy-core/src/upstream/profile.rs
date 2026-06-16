//! Timeout profiles — per-use-type defaults, overrideable via `Custom`.
//!
//! Each `TimeoutProfile` variant maps to a coarse "use type" (chat, quota,
//! oauth, ...) and resolves to a fully-expanded `ResolvedTimeouts` with a
//! numeric value for every pipeline phase. The caller may pass
//! `TimeoutProfile::Custom(ResolvedTimeouts { ... })` to override any
//! value.
//!
//! The default numeric values mirror the existing `TimeoutsConfig` in
//! `crates/openproxy-core/src/timeouts.rs` so behavior is identical to
//! the reqwest path until call sites are migrated one by one.

/// Fully-resolved per-phase timeouts (in milliseconds).
///
/// All values are in milliseconds. The spec defines the phases as
/// `dns` (resolve), `dial` (TCP connect), `tls` (TLS handshake),
/// `write` (request line + headers + body), `headers` (wait for
/// response headers — composes dial+tls+write+wait), `body_chunk`
/// (max gap between two body chunks), and `total` (hard ceiling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedTimeouts {
    pub dns_ms: u64,
    pub dial_ms: u64,
    pub tls_ms: u64,
    pub write_ms: u64,
    pub headers_ms: u64,
    pub body_chunk_ms: u64,
    pub total_ms: u64,
}

impl ResolvedTimeouts {
    /// Sensible production defaults: a few seconds for connection,
    /// a generous chunk gap for streaming, a 5-minute total ceiling.
    ///
    /// These are the system defaults used when no profile override is
    /// provided. They match the `TimeoutsConfig` defaults in
    /// `timeouts.rs::Timeouts::from_config` (as of the current config
    /// schema: connect=5s, request_send=10s, ttft=30s, idle_chunk=120s,
    /// total=300s).
    pub const SYSTEM_DEFAULTS: Self = Self {
        dns_ms: 5_000,
        dial_ms: 5_000, // == `connect_ms` system default
        tls_ms: 5_000,  // rolled into `connect_ms` for backward compat
        write_ms: 10_000, // == `request_send_ms` system default
        headers_ms: 30_000, // == `ttft_ms` system default (wait-for-headers)
        body_chunk_ms: 120_000, // == `idle_chunk_ms` system default
        total_ms: 300_000, // == `total_ms` system default
    };
}

/// Per-use-type default profile. Each variant resolves to a
/// `ResolvedTimeouts` tuned for that workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutProfile {
    /// Chat completion: fast first byte, generous body chunk gap.
    /// Slightly tighter `headers_ms` than the system default to fail
    /// fast when an upstream is dead.
    Chat,
    /// Quota refresh: short overall, no streaming body.
    Quota,
    /// OAuth token refresh: very short, no body.
    OAuth,
    /// Model discovery: long body, lots of JSON parsing.
    ModelDiscovery,
    /// Image generation (future): very long body.
    ImageGeneration,
    /// Caller-supplied: bypasses the per-variant defaults.
    Custom(ResolvedTimeouts),
}

impl TimeoutProfile {
    /// Resolve to a fully-expanded `ResolvedTimeouts`.
    pub fn resolve(&self) -> ResolvedTimeouts {
        match self {
            // Chat: tighten headers (TTFT) to 20s, body chunk gap to 90s.
            // Everything else inherits the system defaults.
            TimeoutProfile::Chat => ResolvedTimeouts {
                headers_ms: 20_000,
                body_chunk_ms: 90_000,
                ..ResolvedTimeouts::SYSTEM_DEFAULTS
            },
            // Quota refresh: aggressive everywhere, no streaming.
            TimeoutProfile::Quota => ResolvedTimeouts {
                dns_ms: 3_000,
                dial_ms: 3_000,
                tls_ms: 3_000,
                write_ms: 5_000,
                headers_ms: 8_000,
                body_chunk_ms: 8_000, // body is small; no gap expected
                total_ms: 15_000,
            },
            // OAuth: very fast, no body.
            TimeoutProfile::OAuth => ResolvedTimeouts {
                dns_ms: 2_000,
                dial_ms: 2_000,
                tls_ms: 2_000,
                write_ms: 3_000,
                headers_ms: 5_000,
                body_chunk_ms: 1_000, // body is tiny; 1s is plenty
                total_ms: 10_000,
            },
            // Model discovery: long body, but headers still tight.
            TimeoutProfile::ModelDiscovery => ResolvedTimeouts {
                headers_ms: 30_000,
                body_chunk_ms: 60_000,
                total_ms: 120_000,
                ..ResolvedTimeouts::SYSTEM_DEFAULTS
            },
            // Image generation: very long.
            TimeoutProfile::ImageGeneration => ResolvedTimeouts {
                headers_ms: 30_000,
                body_chunk_ms: 300_000,
                total_ms: 600_000,
                ..ResolvedTimeouts::SYSTEM_DEFAULTS
            },
            TimeoutProfile::Custom(t) => *t,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_resolves_with_ttft_and_chunk_overrides() {
        let t = TimeoutProfile::Chat.resolve();
        assert_eq!(t.headers_ms, 20_000);
        assert_eq!(t.body_chunk_ms, 90_000);
        // non-overridden phases fall back to system defaults
        assert_eq!(t.total_ms, 300_000);
        assert_eq!(t.dns_ms, 5_000);
    }

    #[test]
    fn oauth_is_aggressive() {
        let t = TimeoutProfile::OAuth.resolve();
        assert!(t.total_ms <= 10_000);
        assert!(t.headers_ms <= 5_000);
    }

    #[test]
    fn custom_passes_through() {
        let custom = ResolvedTimeouts {
            dns_ms: 1,
            dial_ms: 2,
            tls_ms: 3,
            write_ms: 4,
            headers_ms: 5,
            body_chunk_ms: 6,
            total_ms: 7,
        };
        let resolved = TimeoutProfile::Custom(custom).resolve();
        assert_eq!(resolved, custom);
    }
}
