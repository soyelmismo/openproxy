//! Per-phase timeouts for upstream requests with 3-level precedence.
//!
//! Resolution order (highest priority wins):
//! 1. `models.timeout_overrides_json` — per-model, applies to `ttft` and `idle_chunk` only.
//! 2. `provider_timeouts`            — per-provider, applies to `connect`, `request_send`, `total`.
//! 3. System default                  — derived from `TimeoutsConfig` at startup.

use crate::error::{CoreError, Result};
use crate::ids::ProviderId;
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
            Some(s) => Ok(serde_json::from_str(s).map_err(|e| {
                CoreError::Parse(format!("model timeout_overrides_json: {}", e))
            })?),
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
            let connect_ms: u64 = row.get(0).map_err(|e| CoreError::Database {
                message: format!("read connect_ms: {}", e),
                source: None,
            })?;
            let request_send_ms: u64 = row.get(1).map_err(|e| CoreError::Database {
                message: format!("read request_send_ms: {}", e),
                source: None,
            })?;
            let total_ms: u64 = row.get(2).map_err(|e| CoreError::Database {
                message: format!("read total_ms: {}", e),
                source: None,
            })?;
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
}
