//! Per-phase timeouts for upstream requests with 3-level precedence.
//!
//! Resolution order (highest priority wins):
//! 1. `models.timeout_overrides_json` — per-model, applies to `ttft` and `idle_chunk` only.
//! 2. `provider_timeouts`            — per-provider, applies to `connect`, `request_send`, `total`.
//! 3. System default                  — derived from `TimeoutsConfig` at startup.

use crate::error::{CoreError, Result};
use crate::ids::ProviderId;
use crate::upstream::ResolvedTimeouts;
use rusqlite::Connection;
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
/// Connection-level phases are provider-scoped; the total budget is also provider-scoped.
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

/// Per-provider overrides loaded from the `provider_timeouts` table.
#[derive(Debug, Clone, Copy)]
pub struct ProviderTimeouts {
    pub connect: Duration,
    pub request_send: Duration,
    pub total: Duration,
}

impl ProviderTimeouts {
    pub fn from_row(connect_ms: u64, request_send_ms: u64, total_ms: u64) -> Self {
        Self {
            connect: Duration::from_millis(connect_ms),
            request_send: Duration::from_millis(request_send_ms),
            total: Duration::from_millis(total_ms),
        }
    }
}

/// Read provider timeouts for a single provider. Returns `None` when no row exists,
/// signalling the caller to fall back to system defaults.
pub fn load_provider_timeouts(
    conn: &Connection,
    provider: &ProviderId,
) -> Result<Option<ProviderTimeouts>> {
    let mut stmt = conn
        .prepare("SELECT connect_ms, request_send_ms, total_ms FROM provider_timeouts WHERE provider_id = ?1")
        .map_err(|e| CoreError::Database {
            message: format!("prepare load_provider_timeouts: {}", e),
            source: None,
        })?;

    let mut rows = stmt
        .query([provider.as_str()])
        .map_err(|e| CoreError::Database {
            message: format!("query load_provider_timeouts: {}", e),
            source: None,
        })?;

    match rows.next() {
        Ok(Some(row)) => {
            // rusqlite 0.40 removed `FromSql` for `u64` because SQLite
            // INTEGER columns are i64 on the wire. Read as i64 and
            // cast to u64 (these columns store millisecond durations
            // that are always non-negative).
            let connect_ms: u64 = row.get::<_, i64>(0).map_err(|e| CoreError::Database {
                message: format!("read connect_ms: {}", e),
                source: None,
            })? as u64;
            let request_send_ms: u64 = row.get::<_, i64>(1).map_err(|e| CoreError::Database {
                message: format!("read request_send_ms: {}", e),
                source: None,
            })? as u64;
            let total_ms: u64 = row.get::<_, i64>(2).map_err(|e| CoreError::Database {
                message: format!("read total_ms: {}", e),
                source: None,
            })? as u64;
            Ok(Some(ProviderTimeouts::from_row(
                connect_ms,
                request_send_ms,
                total_ms,
            )))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(CoreError::Database {
            message: format!("iterate load_provider_timeouts: {}", e),
            source: None,
        }),
    }
}

/// Resolve the final timeouts for a request by applying 3-level precedence.
///
/// Precedence (later overrides earlier):
/// 1. `defaults`     — system default (`Timeouts::from_config`).
/// 2. `provider`     — per-provider overrides for `connect`, `request_send`, `total`.
/// 3. `model_overrides` — per-model overrides for `ttft`, `idle_chunk` only.
pub fn resolve(
    defaults: &Timeouts,
    provider: Option<&ProviderTimeouts>,
    model_overrides: Option<&ModelTimeoutOverrides>,
) -> Timeouts {
    let mut t = *defaults;
    if let Some(p) = provider {
        t.connect = p.connect;
        t.request_send = p.request_send;
        t.total = p.total;
    }
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
        let resolved = resolve(&d, None, None);
        assert_eq!(resolved.connect, d.connect);
        assert_eq!(resolved.request_send, d.request_send);
        assert_eq!(resolved.ttft, d.ttft);
        assert_eq!(resolved.idle_chunk, d.idle_chunk);
        assert_eq!(resolved.total, d.total);
    }

    #[test]
    fn resolve_provider_overrides_connect_send_total() {
        let d = defaults();
        let provider = ProviderTimeouts::from_row(1_000, 2_000, 60_000);
        let resolved = resolve(&d, Some(&provider), None);

        assert_eq!(resolved.connect, Duration::from_millis(1_000));
        assert_eq!(resolved.request_send, Duration::from_millis(2_000));
        assert_eq!(resolved.total, Duration::from_millis(60_000));

        // Provider must NOT touch streaming phases.
        assert_eq!(resolved.ttft, d.ttft);
        assert_eq!(resolved.idle_chunk, d.idle_chunk);

        // Model overrides with no fields set must not affect provider phases either.
        let empty_model = ModelTimeoutOverrides::default();
        let resolved2 = resolve(&d, Some(&provider), Some(&empty_model));
        assert_eq!(resolved2.connect, resolved.connect);
        assert_eq!(resolved2.request_send, resolved.request_send);
        assert_eq!(resolved2.total, resolved.total);
    }

    #[test]
    fn resolve_model_overrides_only_affects_ttft_and_idle() {
        let d = defaults();
        let model = ModelTimeoutOverrides {
            ttft_ms: Some(7_000),
            idle_chunk_ms: Some(45_000),
        };
        let resolved = resolve(&d, None, Some(&model));

        assert_eq!(resolved.ttft, Duration::from_millis(7_000));
        assert_eq!(resolved.idle_chunk, Duration::from_millis(45_000));

        // Model overrides must NOT touch provider phases.
        assert_eq!(resolved.connect, d.connect);
        assert_eq!(resolved.request_send, d.request_send);
        assert_eq!(resolved.total, d.total);
    }

    #[test]
    fn resolve_full_precedence() {
        let d = defaults();
        let provider = ProviderTimeouts::from_row(2_000, 4_000, 90_000);
        let model = ModelTimeoutOverrides {
            ttft_ms: Some(5_000),
            idle_chunk_ms: Some(60_000),
        };
        let resolved = resolve(&d, Some(&provider), Some(&model));

        // provider wins for connect/request_send/total
        assert_eq!(resolved.connect, Duration::from_millis(2_000));
        assert_eq!(resolved.request_send, Duration::from_millis(4_000));
        assert_eq!(resolved.total, Duration::from_millis(90_000));
        // model wins for ttft/idle_chunk
        assert_eq!(resolved.ttft, Duration::from_millis(5_000));
        assert_eq!(resolved.idle_chunk, Duration::from_millis(60_000));
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
