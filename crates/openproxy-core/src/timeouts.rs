//! Per-phase timeouts for upstream requests with 2-level precedence.
//!
//! Resolution order (highest priority wins):
//! 1. `models.timeout_overrides_json` — per-model, applies to `ttft` and `idle_chunk` only.
//! 2. System default                  — derived from `TimeoutsConfig` at startup.
//!
//! The per-provider `provider_timeouts` table has been REMOVED. The global
//! `TimeoutsConfig` (set via the dashboard's Config page) is the single
//! source of truth for `connect`, `request_send`, and `total` across all
//! providers. Per-model overrides still apply to `ttft` and `idle_chunk`
//! (streaming-relevant phases) via the `models.timeout_overrides_json`
//! column.

use crate::error::{CoreError, Result};
use crate::upstream::ResolvedTimeouts;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Final, fully-resolved timeouts for a single upstream request.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Timeouts {
    pub connect: Duration,
    pub request_send: Duration,
    pub ttft: Duration,
    pub idle_chunk: Duration,
    pub total: Duration,
}

impl Timeouts {
    /// Build a `Timeouts` from the system-default `TimeoutsConfig`.
    pub fn from_config(c: &crate::config::TimeoutsConfig) -> Self {
        Self {
            connect: Duration::from_millis(c.connect_ms),
            request_send: Duration::from_millis(c.request_send_ms),
            ttft: Duration::from_millis(c.ttft_ms),
            idle_chunk: Duration::from_millis(c.idle_chunk_ms),
            total: Duration::from_millis(c.total_ms),
        }
    }

    /// Map the pipeline-level `Timeouts` shape to the
    /// `upstream::profile::ResolvedTimeouts` shape used by
    /// `UpstreamClient::call`.
    ///
    /// Mapping (see `upstream-migration-report.md` §1 + task spec):
    /// - `dns_ms`     = `connect_ms / 2`     (best-effort split of the
    ///   single connect timeout into a DNS sub-phase)
    /// - `dial_ms`    = `connect_ms`         (TCP connect inherits the
    ///   full budget; hyper's default connector doesn't separate dial
    ///   from TLS)
    /// - `tls_ms`     = `connect_ms`         (TLS inherits the full
    ///   budget, same rationale)
    /// - `write_ms`   = `request_send_ms`
    /// - `headers_ms` = `ttft_ms`
    /// - `body_chunk_ms` = `idle_chunk_ms`
    /// - `total_ms`   = `total_ms`
    ///
    /// Note: hyper's `DefaultConnector` does not separate dial from
    /// TLS in the `Service::call` future, so a stalled connector is
    /// attributed to a single phase boundary (`Headers`) by the
    /// production dispatch. Splitting connect from TLS in production
    /// requires a custom DNS resolver and is a follow-up gate.
    pub fn as_resolved(&self) -> ResolvedTimeouts {
        ResolvedTimeouts {
            dns_ms: self.connect.as_millis() as u64 / 2,
            dial_ms: self.connect.as_millis() as u64,
            tls_ms: self.connect.as_millis() as u64,
            write_ms: self.request_send.as_millis() as u64,
            headers_ms: self.ttft.as_millis() as u64,
            body_chunk_ms: self.idle_chunk.as_millis() as u64,
            total_ms: self.total.as_millis() as u64,
        }
    }
}

/// Per-model override stored as JSON in `models.timeout_overrides_json`.
///
/// Only streaming-relevant phases are overridable per model: `ttft` and `idle_chunk`.
/// Connection-level phases (`connect`, `request_send`, `total`) are NOT overridable
/// per model — they always come from the global `TimeoutsConfig`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelTimeoutOverrides {
    pub ttft_ms: Option<u64>,
    pub idle_chunk_ms: Option<u64>,
}

impl ModelTimeoutOverrides {
    /// Parse a JSON column value into overrides. `None` or empty string means "no overrides".
    pub fn from_json(s: Option<&str>) -> Result<Self> {
        match s {
            None | Some("") => Ok(Self::default()),
            Some(s) => Ok(serde_json::from_str(s)
                .map_err(|e| CoreError::Parse(format!("model timeout_overrides_json: {}", e)))?),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.ttft_ms.is_none() && self.idle_chunk_ms.is_none()
    }
}

/// Resolve the final timeouts for a request by applying 2-level precedence.
///
/// Precedence (later overrides earlier):
/// 1. `defaults`        — system default (`Timeouts::from_config`). The global
///                        `TimeoutsConfig` is the single source of truth for
///                        `connect`, `request_send`, and `total`.
/// 2. `model_overrides` — per-model overrides for `ttft`, `idle_chunk` only.
pub fn resolve(defaults: &Timeouts, model_overrides: Option<&ModelTimeoutOverrides>) -> Timeouts {
    let mut t = *defaults;
    if let Some(m) = model_overrides {
        if let Some(ms) = m.ttft_ms {
            t.ttft = Duration::from_millis(ms);
        }
        if let Some(ms) = m.idle_chunk_ms {
            t.idle_chunk = Duration::from_millis(ms);
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Timeouts {
        Timeouts {
            connect: Duration::from_millis(5_000),
            request_send: Duration::from_millis(10_000),
            ttft: Duration::from_millis(30_000),
            idle_chunk: Duration::from_millis(120_000),
            total: Duration::from_millis(300_000),
        }
    }

    #[test]
    fn resolve_uses_defaults_when_no_overrides() {
        let d = defaults();
        let resolved = resolve(&d, None);
        assert_eq!(resolved.connect, d.connect);
        assert_eq!(resolved.request_send, d.request_send);
        assert_eq!(resolved.ttft, d.ttft);
        assert_eq!(resolved.idle_chunk, d.idle_chunk);
        assert_eq!(resolved.total, d.total);
    }

    #[test]
    fn resolve_model_overrides_only_affects_ttft_and_idle() {
        let d = defaults();
        let model = ModelTimeoutOverrides {
            ttft_ms: Some(7_000),
            idle_chunk_ms: Some(45_000),
        };
        let resolved = resolve(&d, Some(&model));

        assert_eq!(resolved.ttft, Duration::from_millis(7_000));
        assert_eq!(resolved.idle_chunk, Duration::from_millis(45_000));

        // Model overrides must NOT touch connection-level phases —
        // those always come from the global config.
        assert_eq!(resolved.connect, d.connect);
        assert_eq!(resolved.request_send, d.request_send);
        assert_eq!(resolved.total, d.total);
    }

    #[test]
    fn resolve_empty_model_overrides_is_noop() {
        let d = defaults();
        let empty_model = ModelTimeoutOverrides::default();
        let resolved = resolve(&d, Some(&empty_model));

        assert_eq!(resolved.connect, d.connect);
        assert_eq!(resolved.request_send, d.request_send);
        assert_eq!(resolved.ttft, d.ttft);
        assert_eq!(resolved.idle_chunk, d.idle_chunk);
        assert_eq!(resolved.total, d.total);
    }

    #[test]
    fn resolve_partial_model_overrides() {
        let d = defaults();
        // Only override ttft, leave idle_chunk at the default.
        let model = ModelTimeoutOverrides {
            ttft_ms: Some(15_000),
            idle_chunk_ms: None,
        };
        let resolved = resolve(&d, Some(&model));

        assert_eq!(resolved.ttft, Duration::from_millis(15_000));
        assert_eq!(resolved.idle_chunk, d.idle_chunk);
        assert_eq!(resolved.connect, d.connect);
        assert_eq!(resolved.request_send, d.request_send);
        assert_eq!(resolved.total, d.total);
    }

    #[test]
    fn model_overrides_from_json_parses() {
        let original = ModelTimeoutOverrides {
            ttft_ms: Some(1_234),
            idle_chunk_ms: Some(56_789),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed = ModelTimeoutOverrides::from_json(Some(&json)).unwrap();
        assert_eq!(parsed.ttft_ms, original.ttft_ms);
        assert_eq!(parsed.idle_chunk_ms, original.idle_chunk_ms);
        assert!(!parsed.is_empty());

        // Roundtrip: serialize the parsed value again and compare structurally.
        let json2 = serde_json::to_string(&parsed).unwrap();
        let parsed2 = ModelTimeoutOverrides::from_json(Some(&json2)).unwrap();
        assert_eq!(parsed2.ttft_ms, original.ttft_ms);
        assert_eq!(parsed2.idle_chunk_ms, original.idle_chunk_ms);
    }

    #[test]
    fn model_overrides_from_empty_string_returns_default() {
        let none = ModelTimeoutOverrides::from_json(None).unwrap();
        assert!(none.is_empty());
        assert!(none.ttft_ms.is_none());
        assert!(none.idle_chunk_ms.is_none());

        let empty = ModelTimeoutOverrides::from_json(Some("")).unwrap();
        assert!(empty.is_empty());
        assert!(empty.ttft_ms.is_none());
        assert!(empty.idle_chunk_ms.is_none());
    }

    #[test]
    fn as_resolved_maps_pipeline_timeouts_to_upstream_phases() {
        // Use the system defaults: connect=5s, request_send=10s,
        // ttft=30s, idle_chunk=120s, total=300s.
        let t = defaults();
        let r = t.as_resolved();
        // connect -> dns (half), dial, tls.
        assert_eq!(r.dns_ms, 2_500);
        assert_eq!(r.dial_ms, 5_000);
        assert_eq!(r.tls_ms, 5_000);
        // 1:1 mappings.
        assert_eq!(r.write_ms, 10_000);
        assert_eq!(r.headers_ms, 30_000);
        assert_eq!(r.body_chunk_ms, 120_000);
        assert_eq!(r.total_ms, 300_000);
    }

    #[test]
    fn as_resolved_handles_zero_connect() {
        // Edge case: a tight connect budget rounds down to 0 ms DNS,
        // which the upstream client treats as "no DNS budget" (it
        // races against the deadline; an instant deadline is OK).
        let t = Timeouts {
            connect: Duration::from_millis(1),
            request_send: Duration::from_millis(2),
            ttft: Duration::from_millis(3),
            idle_chunk: Duration::from_millis(4),
            total: Duration::from_millis(5),
        };
        let r = t.as_resolved();
        assert_eq!(r.dns_ms, 0);
        assert_eq!(r.dial_ms, 1);
        assert_eq!(r.tls_ms, 1);
        assert_eq!(r.write_ms, 2);
        assert_eq!(r.headers_ms, 3);
        assert_eq!(r.body_chunk_ms, 4);
        assert_eq!(r.total_ms, 5);
    }
}
