//! Per-phase timeouts for upstream requests with 2-level precedence.

use openproxy_adapters::upstream::ResolvedTimeouts;
use openproxy_types::error::{CoreError, Result};
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
    pub fn from_config(c: &openproxy_types::config::TimeoutsConfig) -> Self {
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

    #[test]
    fn test_timeout_resolution() {
        let defaults = Timeouts {
            connect: Duration::from_millis(100),
            request_send: Duration::from_millis(200),
            ttft: Duration::from_millis(300),
            idle_chunk: Duration::from_millis(400),
            total: Duration::from_millis(500),
        };

        // No overrides
        let resolved = resolve(&defaults, None);
        assert_eq!(resolved.ttft, Duration::from_millis(300));
        assert_eq!(resolved.idle_chunk, Duration::from_millis(400));

        // With overrides
        let overrides = ModelTimeoutOverrides {
            ttft_ms: Some(1000),
            idle_chunk_ms: None,
        };
        let resolved2 = resolve(&defaults, Some(&overrides));
        assert_eq!(resolved2.ttft, Duration::from_millis(1000));
        assert_eq!(resolved2.idle_chunk, Duration::from_millis(400));
    }
}
