//! Devin Connect-RPC frame processing for the streaming pipeline.
//!
//! Devin upstreams emit length-prefixed protobuf envelopes instead of
//! newline-delimited SSE. The stream loop feeds each complete frame here as
//! `flag || payload` bytes; this module converts them into the same
//! [`UpstreamSseChunk`] flow the translated (Gemini/Anthropic) formats use,
//! so PII, normalization, accumulation and sink delivery are shared.

use super::StreamContext;
use super::processor::{ChunkProcessor, record_ttft_if_first};
use crate::sse::devin as devin_sse;
use crate::streaming::ChunkEvent;
use openproxy_adapters::adapters::devin::wire;
use openproxy_types::error::CoreError;

/// Connect end-of-stream trailer flag (mask test: compressed trailers keep
/// bit 0 set).
const CONNECT_END_STREAM_MASK: u8 = 0x02;

impl ChunkProcessor<'_> {
    /// Handles one complete Devin frame (flag byte + protobuf payload).
    ///
    /// End-stream frames carry a JSON trailer: `{}` or non-JSON means a
    /// clean finish, `{"error":{...}}` maps to the recorded upstream error
    /// path. Data frames parse into `DevinFrame` and forward as chunks.
    pub(super) async fn process_devin_frame(
        &mut self,
        ctx: &StreamContext<'_>,
        stream: &mut openproxy_adapters::upstream::UpstreamBodyStream,
        frame_bytes: &[u8],
    ) -> Result<ChunkEvent, CoreError> {
        let Some((&flag, payload)) = frame_bytes.split_first() else {
            return Ok(ChunkEvent::Skip);
        };

        if flag & CONNECT_END_STREAM_MASK != 0 {
            return self.handle_devin_end_stream(ctx, payload).await;
        }

        record_ttft_if_first(ctx, self.state);

        let frame = match wire::parse_devin_frame(payload) {
            Ok(frame) => frame,
            Err(e) => {
                let err = CoreError::UpstreamConnection(format!("devin frame parse: {e}"));
                return self.devin_fail_with_trace(ctx, &err).await;
            }
        };

        match devin_sse::devin_frame_to_chunk(
            &mut self.state.devin,
            &frame,
            ctx.chunk_id,
            ctx.created,
            ctx.model_name,
        ) {
            Some(chunk) => self.handle_translated_chunk(ctx, stream, chunk).await,
            None => Ok(ChunkEvent::Skip),
        }
    }

    async fn handle_devin_end_stream(
        &mut self,
        ctx: &StreamContext<'_>,
        payload: &[u8],
    ) -> Result<ChunkEvent, CoreError> {
        if let Some((status, msg)) = wire::parse_devin_trailer_error(payload) {
            let class = crate::error_classification::classify_upstream_error(status, &msg);
            let err = CoreError::UpstreamError {
                status,
                provider: ctx.target.provider_id.to_string(),
                model: ctx.model_name.to_string(),
                body: msg,
                is_proxy_rotated: false,
                class,
                is_hard_skip: class.is_hard_skip(),
            };
            return self.devin_fail_with_trace(ctx, &err).await;
        }

        let chunk = devin_sse::devin_done_chunk(
            &self.state.devin,
            ctx.chunk_id,
            ctx.created,
            ctx.model_name,
        );
        self.handle_translated_done(ctx, chunk).await
    }

    /// Mirrors the translated-format parse-error path: reasoning summary,
    /// accumulator partial mark and the dispatcher failure record.
    async fn devin_fail_with_trace(
        &mut self,
        ctx: &StreamContext<'_>,
        err: &CoreError,
    ) -> Result<ChunkEvent, CoreError> {
        tracing::warn!(
            chunk_id = %ctx.chunk_id,
            error = %err,
            "failed to parse devin frame from upstream"
        );
        self.emit_reasoning_summary(ctx);
        let acc_ref = self.state.acc.as_mut().map(|a| {
            a.mark_partial();
            &*a
        });
        Ok(ChunkEvent::Return(Box::new(
            self.dispatcher
                .record_and_fail_with_trace_id_and_partial(crate::PartialFailureParams {
                    req: ctx.req.to_owned(),
                    combo: ctx.combo,
                    target: ctx.target,
                    ctx: crate::FailureContext {
                        proxy_url: ctx.proxy_url.clone(),
                        proxy_status: ctx.proxy_status.clone(),
                        attempt: ctx.attempt,
                        race_size: ctx.race_size,
                        err,
                        started: ctx.started,
                        model: Some(ctx.model),
                        connect_ms: Some(ctx.connect_and_send_ms),
                        ttft_ms: self.state.ttft_ms,
                        status_code: err.http_status(),
                    },
                    trace_id: ctx.trace_id.to_string(),
                    acc: acc_ref,
                    chunk_id: Some(ctx.chunk_id),
                    created: ctx.created,
                    model_name: ctx.model_name,
                })
                .await,
        )))
    }
}
