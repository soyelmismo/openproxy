use crate::PipelineRequest;
use openproxy_types::combos::{Combo, ComboTarget};
use openproxy_types::error::CoreError;
use openproxy_types::models::Model;
use std::time::Instant;

use crate::timeouts::Timeouts;
use openproxy_adapters::upstream::UpstreamRequest;

/// Bundles the parameters shared by streaming failure methods
/// (`fail_stream_client_disconnected`, `fail_on_sink_send_error`).
/// Eliminates la anti-pattern de 14-15 argumentos posicionales.
pub(crate) struct DispatchContext<'a> {
    pub(crate) attempt: u8,
    pub(crate) race_size: u8,
    pub(crate) started: Instant,
    pub(crate) model: &'a Model,
    pub(crate) proxy_url: Option<String>,
    pub(crate) proxy_status: Option<String>,
}

impl<'a> DispatchContext<'a> {
    #[inline]
    pub(crate) fn fail_ctx_code<'e>(
        &self,
        err: &'e CoreError,
        connect_ms: Option<u64>,
        ttft_ms: Option<u64>,
        status_code: u16,
    ) -> crate::FailureContext<'e>
    where
        'a: 'e,
    {
        crate::FailureContext {
            proxy_url: self.proxy_url.clone(),
            proxy_status: self.proxy_status.clone(),
            attempt: self.attempt,
            race_size: self.race_size,
            err,
            started: self.started,
            model: Some(self.model),
            connect_ms,
            ttft_ms,
            status_code,
        }
    }
}

pub(crate) struct StreamFailureContext<'a> {
    pub(crate) req: PipelineRequest,
    pub(crate) combo: &'a Combo,
    pub(crate) target: &'a ComboTarget,
    pub(crate) attempt: u8,
    pub(crate) race_size: u8,
    pub(crate) started: std::time::Instant,
    pub(crate) model: &'a Model,
    pub(crate) connect_ms: u64,
    pub(crate) ttft_ms: Option<u64>,
    pub(crate) trace_id: String,
    pub(crate) acc: Option<&'a mut crate::sse_accumulator::ResponseAccumulator>,
    pub(crate) chunk_id: &'a str,
    pub(crate) created: u64,
    pub(crate) model_name: &'a str,
    pub(crate) proxy_url: Option<String>,
    pub(crate) proxy_status: Option<String>,
}

pub(crate) struct DispatchParams<'a> {
    pub target: &'a ComboTarget,
    pub combo: &'a Combo,
    pub req: PipelineRequest,
    pub model: &'a Model,
    pub target_format: openproxy_types::TargetFormat,
    pub url: &'a str,
    pub headers: &'a [(String, String)],
    pub body_bytes: bytes::Bytes,
    pub resolved_timeouts: &'a Timeouts,
    pub started: Instant,
    pub attempt: u8,
    pub race_size: u8,
    pub trace_id: String,
}

pub(crate) struct StreamDispatchParams<'a> {
    pub target: &'a ComboTarget,
    pub combo: &'a Combo,
    pub req: PipelineRequest,
    pub model: &'a Model,
    pub target_format: openproxy_types::TargetFormat,
    pub resolved_timeouts: &'a Timeouts,
    pub started: Instant,
    pub attempt: u8,
    pub race_size: u8,
    pub trace_id: String,
    pub upstream_request: UpstreamRequest,
}
