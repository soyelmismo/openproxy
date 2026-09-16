use super::StreamContext;
use super::processor::ChunkProcessor;
use crate::FailureContext;
use crate::sse::merge_usage;
use crate::streaming::{StreamAction, StreamingChunkStage};
use openproxy_types::error::CoreError;

impl ChunkProcessor<'_> {
    pub(super) fn check_and_handle_inline_upstream_error(
        &mut self,
        ctx: &StreamContext<'_>,
        json_payload: &str,
    ) -> Option<crate::streaming::ChunkEvent> {
        let parsed = crate::sse::parse_inline_sse_error(json_payload)?;
        let provider_name = parsed.provider.unwrap_or(ctx.target.provider_id.as_str());
        let code = parsed.status_code;
        let message = parsed.message;

        tracing::warn!(
            combo_id = ctx.combo.id.0,
            target_id = ctx.target.id.0,
            provider = %provider_name,
            code,
            message,
            "upstream error embedded in streaming chunk"
        );
        let class = crate::error_classification::classify_upstream_error(code, message);
        let err = CoreError::upstream_error_classified(
            code,
            provider_name,
            ctx.model_name,
            message,
            false,
            class,
        );
        let acc_ref: Option<&crate::sse_accumulator::ResponseAccumulator> =
            match &mut self.state.acc {
                Some(a) => {
                    a.mark_partial();
                    Some(&*a)
                }
                None => None,
            };
        Some(crate::streaming::ChunkEvent::Return(Box::new(
            self.dispatcher.record_and_fail_with_trace_id_and_partial(
                crate::PartialFailureParams {
                    req: ctx.req.to_owned(),
                    combo: ctx.combo,
                    target: ctx.target,
                    ctx: FailureContext {
                        proxy_url: ctx.proxy_url.clone(),
                        proxy_status: ctx.proxy_status.clone(),
                        attempt: ctx.attempt,
                        race_size: ctx.race_size,
                        err: &err,
                        started: ctx.started,
                        model: Some(ctx.model),
                        connect_ms: Some(ctx.connect_and_send_ms),
                        ttft_ms: self.state.ttft_ms,
                        status_code: code,
                    },
                    trace_id: ctx.trace_id.to_string(),
                    acc: acc_ref,
                    chunk_id: Some(ctx.chunk_id),
                    created: ctx.created,
                    model_name: ctx.model_name,
                },
            ),
        )))
    }

    pub(super) fn update_state_and_acc_from_metadata_chunk(
        &mut self,
        mut chunk: crate::sse::UpstreamSseChunk,
        json_payload: &str,
    ) {
        if let Some(new_usage) = chunk.usage.take() {
            self.state.usage = Some(match self.state.usage.take() {
                Some(existing) => merge_usage(existing, new_usage),
                None => new_usage,
            });
        }
        if chunk.stop_reason.is_some() && self.state.stop_reason.is_none() {
            self.state.stop_reason = chunk.stop_reason.take();
        }

        let effective_payload = match self.state.normalizer.process_chunk(json_payload) {
            StreamAction::Mutate(s) => Some(s),
            _ => None,
        };
        let payload_str = effective_payload.as_deref().unwrap_or(json_payload);
        if let Some(a) = self.state.acc.as_mut() {
            if let Some(u) = &self.state.usage {
                a.set_usage(u.to_owned());
            }
            if let Some(sr) = &self.state.stop_reason {
                a.set_stop_reason(sr);
            }
            a.process_chunk(payload_str);
            let _ = chunk.delta_reasoning.take();
        }
    }

    pub(super) async fn process_openai_metadata_chunk(
        &mut self,
        ctx: &StreamContext<'_>,
        stream: &mut openproxy_adapters::upstream::UpstreamBodyStream,
        line: &str,
        json_payload: &str,
    ) -> Result<crate::streaming::ChunkEvent, CoreError> {
        let chunk = match crate::sse::parse_openai_sse_line(line) {
            Ok(Some(chunk)) => chunk,
            Ok(None) => return Ok(crate::streaming::ChunkEvent::Skip),
            Err(e) => {
                tracing::warn!(
                    chunk_id = %ctx.chunk_id,
                    error = %e,
                    "failed to parse SSE line from upstream"
                );
                return Ok(crate::streaming::ChunkEvent::Skip);
            }
        };

        let has_content = chunk.has_content;
        self.update_state_and_acc_from_metadata_chunk(chunk, json_payload);

        if let Some(event) = self.check_race_cancelled(ctx) {
            return Ok(event);
        }

        if has_content {
            stream.note_content_chunk();
        }

        let effective_payload = match self.state.normalizer.process_chunk(json_payload) {
            StreamAction::Mutate(s) => Some(s),
            _ => None,
        };
        let payload_str = effective_payload.as_deref().unwrap_or(json_payload);

        let pii_action = match &mut self.state.pii_stage {
            Some(stage) => stage.process_chunk(payload_str),
            None => StreamAction::Passthrough,
        };
        let final_payload = match &pii_action {
            StreamAction::Mutate(s) => s.as_str(),
            _ => payload_str,
        };

        let sse_bytes = crate::sse::build_sse_frame(final_payload);

        if let Err(e) = self.send_to_sink(ctx, sse_bytes).await {
            let fail_ctx = self.state.make_failure_context(ctx);
            return Ok(crate::streaming::ChunkEvent::Return(Box::new(
                self.dispatcher.fail_on_sink_send_error(e, fail_ctx),
            )));
        }

        Ok(crate::streaming::ChunkEvent::Skip)
    }

    pub(super) fn prepare_content_chunk_bytes(
        &mut self,
        json_payload: &str,
        line_bytes: &[u8],
    ) -> bytes::Bytes {
        let norm_action = self.state.normalizer.process_chunk(json_payload);
        let normalized_payload = match &norm_action {
            StreamAction::Mutate(s) => s.as_str(),
            _ => json_payload,
        };

        if let Some(a) = self.state.acc.as_mut() {
            a.process_chunk(normalized_payload);
        }

        let pii_action = match &mut self.state.pii_stage {
            Some(stage) => stage.process_chunk(normalized_payload),
            None => StreamAction::Passthrough,
        };

        match pii_action {
            StreamAction::Mutate(modified) => crate::sse::build_sse_frame(&modified),
            StreamAction::Skip => bytes::Bytes::new(),
            _ => match norm_action {
                StreamAction::Mutate(modified) => crate::sse::build_sse_frame(&modified),
                _ => {
                    let mut frame = bytes::BytesMut::from(line_bytes);
                    frame.extend_from_slice(b"\n\n");
                    frame.freeze()
                }
            },
        }
    }

    pub(super) async fn process_openai_content_chunk(
        &mut self,
        ctx: &StreamContext<'_>,
        stream: &mut openproxy_adapters::upstream::UpstreamBodyStream,
        json_payload: &str,
        line_bytes: &[u8],
    ) -> Result<crate::streaming::ChunkEvent, CoreError> {
        let sse_bytes = self.prepare_content_chunk_bytes(json_payload, line_bytes);

        if let Some(event) = self.check_race_cancelled(ctx) {
            return Ok(event);
        }

        stream.note_content_chunk();
        if let Err(e) = self.send_to_sink(ctx, sse_bytes).await {
            let fail_ctx = self.state.make_failure_context(ctx);
            return Ok(crate::streaming::ChunkEvent::Return(Box::new(
                self.dispatcher.fail_on_sink_send_error(e, fail_ctx),
            )));
        }

        Ok(crate::streaming::ChunkEvent::Skip)
    }

    /// OpenAI-format SSE handler (fast + slow paths).
    pub(super) async fn process_openai_format(
        &mut self,
        ctx: &StreamContext<'_>,
        stream: &mut openproxy_adapters::upstream::UpstreamBodyStream,
        line: &str,
        line_bytes: &[u8],
    ) -> Result<crate::streaming::ChunkEvent, CoreError> {
        if line_bytes.len() < 5 || &line_bytes[..5] != b"data:" {
            return Ok(crate::streaming::ChunkEvent::Skip);
        }
        let payload_bytes = &line_bytes[5..];
        let json_payload_bytes = crate::sse::skip_leading_spaces(payload_bytes);
        let json_payload = std::str::from_utf8(json_payload_bytes).unwrap_or("");
        let json_payload = json_payload.trim_end_matches(['\r', '\n', ' ']);

        if json_payload == "[DONE]" {
            return self.handle_done_sentinel(ctx).await;
        }

        if let Some(ret) = self.check_and_handle_inline_upstream_error(ctx, json_payload) {
            return Ok(ret);
        }

        if crate::sse::sse_payload_needs_parse(json_payload) {
            self.process_openai_metadata_chunk(ctx, stream, line, json_payload)
                .await
        } else {
            self.process_openai_content_chunk(ctx, stream, json_payload, line_bytes)
                .await
        }
    }
}
