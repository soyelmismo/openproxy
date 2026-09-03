//! Tipos compartidos por todos los submódulos del dispatcher. Cero lógica de
//! negocio: structs de datos puros más el helper `DispatchContext::fail_ctx_code`
//! que construye un `FailureContext` (la única función permitida en este
//! archivo porque forma parte inseparable del tipo `DispatchContext`).

use crate::timeouts::Timeouts;
use openproxy_adapters::upstream::UpstreamRequest;
use openproxy_types::models::Model;
use openproxy_types::TargetFormat;
use std::time::Instant;

use crate::PipelineRequest;

/// Context de un único intento de dispatch, compartido entre las rutas
/// streaming y no-streaming. Permanece en memoria solo durante la vida del
/// intento (no se persiste).
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
        err: &'e openproxy_types::error::CoreError,
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

/// Contexto ampliado para fallos dentro del loop SSE streaming. Acepta
/// referencias mutables al accumulator (`acc`) para marcar la respuesta como
/// parcial antes de devolver el `PipelineResult`.
pub(crate) struct StreamFailureContext<'a> {
    pub(crate) req: PipelineRequest,
    pub(crate) combo: &'a openproxy_types::combos::Combo,
    pub(crate) target: &'a openproxy_types::combos::ComboTarget,
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

/// Parámetros del entry-point `dispatch_upstream` (rama no-streaming
/// principal). Reune 13 campos para evitar `too_many_arguments`.
pub(crate) struct DispatchParams<'a> {
    pub target: &'a openproxy_types::combos::ComboTarget,
    pub combo: &'a openproxy_types::combos::Combo,
    pub req: PipelineRequest,
    pub model: &'a Model,
    pub target_format: TargetFormat,
    pub url: &'a str,
    pub headers: &'a [(String, String)],
    pub body_bytes: bytes::Bytes,
    pub resolved_timeouts: &'a Timeouts,
    pub started: Instant,
    pub attempt: u8,
    pub race_size: u8,
    pub trace_id: String,
}

/// Argumentos empaquetados para `record_non_streaming_success` tras un 2xx.
pub(crate) struct NonStreamingSuccessArgs {
    pub status_code: u16,
    pub connect_and_send_ms: u64,
    pub ttft_ms: u64,
    pub response_headers: Option<std::collections::BTreeMap<String, String>>,
    pub response_body_raw: serde_json::Value,
    pub openai_response: crate::translation::OpenAIResponse,
}

/// Parámetros del entry-point `dispatch_upstream_streaming`. Contiene el
/// `UpstreamRequest` ya construido por `setup_upstream_request_and_context`.
pub(crate) struct StreamDispatchParams<'a> {
    pub target: &'a openproxy_types::combos::ComboTarget,
    pub combo: &'a openproxy_types::combos::Combo,
    pub req: PipelineRequest,
    pub model: &'a Model,
    pub target_format: TargetFormat,
    pub resolved_timeouts: &'a Timeouts,
    pub started: Instant,
    pub attempt: u8,
    pub race_size: u8,
    pub trace_id: String,
    pub upstream_request: UpstreamRequest,
}

/// Argumentos para `handle_streaming_non_2xx` cuando la respuesta inicial
/// no es 2xx pero el body ya está disponible.
pub(crate) struct StreamingNon2xxArgs<'a> {
    pub response: openproxy_adapters::upstream::UpstreamResponse,
    pub status_code: u16,
    pub req: PipelineRequest,
    pub combo: &'a openproxy_types::combos::Combo,
    pub target: &'a openproxy_types::combos::ComboTarget,
    pub model: &'a Model,
    pub connect_and_send_ms: u64,
}

/// Argumentos finales para `record_streaming_success` tras consumir el
/// stream completo.
pub(crate) struct StreamingSuccessArgs<'a> {
    pub state: crate::streaming_state::StreamingState,
    pub chunk_id: &'a str,
    pub created: u64,
    pub model_name: &'a str,
    pub connect_and_send_ms: u64,
    pub status_code: u16,
}