use super::processor::ChunkProcessor;
use super::{StreamContext, StreamingState};
use crate::SSE_DONE_BYTES;
use crate::sse::merge_usage;
use crate::streaming::{StreamAction, StreamingChunkStage};
use openproxy_types::error::CoreError;

pub(super) fn parse_translated_sse_line(
    state: &mut StreamingState,
    target_format: openproxy_types::TargetFormat,
    line: &str,
    chunk_id: &str,
    created: u64,
    model_name: &str,
) -> Result<Option<crate::sse::UpstreamSseChunk>, CoreError> {
    match target_format {
        openproxy_types::TargetFormat::Responses => crate::sse::parse_responses_sse_stream_line(
            line,
            chunk_id,
            created,
            model_name,
            &mut state.responses_sse_state,
        ),
        openproxy_types::TargetFormat::Openai => crate::sse::parse_openai_sse_line(line),
        openproxy_types::TargetFormat::CommandCodeGo => crate::sse::parse_commandcode_sse_line(
            line,
            chunk_id,
            created,
            model_name,
            &mut state.commandcode_sse_state,
        ),
        openproxy_types::TargetFormat::Atomesus => {
            crate::sse::parse_atomesus_sse_line(line, chunk_id, created, model_name)
        }
        openproxy_types::TargetFormat::Gemini => {
            crate::sse::parse_gemini_sse_line(line, chunk_id, created, model_name)
        }
        openproxy_types::TargetFormat::Anthropic => {
            let Some(payload) =
                crate::sse::parse_anthropic_sse_stream_line(line, &mut state.current_event_type)?
            else {
                return Ok(None);
            };
            crate::sse::translate_anthropic_sse_event(
                &payload,
                chunk_id,
                created,
                model_name,
                &mut state.tool_use_acc,
                &mut state.tool_call_index_counter,
            )
        }
        openproxy_types::TargetFormat::SystemOne => Ok(None),
    }
}

impl ChunkProcessor<'_> {
    pub(super) async fn handle_translated_done(
        &mut self,
        ctx: &StreamContext<'_>,
        mut chunk: crate::sse::UpstreamSseChunk,
    ) -> Result<crate::streaming::ChunkEvent, CoreError> {
        if let Some(new_usage) = chunk.usage.take() {
            self.state.usage = Some(match self.state.usage.take() {
                Some(existing) => merge_usage(existing, new_usage),
                None => new_usage,
            });
        }
        if chunk.stop_reason.is_some() {
            self.state.stop_reason = chunk.stop_reason.take();
        }
        let json_str = chunk.into_json_string();

        if let Some(a) = self.state.acc.as_mut() {
            if let Some(u) = &self.state.usage {
                a.set_usage(u.to_owned());
            }
            if let Some(sr) = &self.state.stop_reason {
                a.set_stop_reason(sr);
            }
            a.append_openai_raw(&json_str);
        }

        if let Some(cancel) = self.check_race_cancelled(ctx) {
            return Ok(cancel);
        }

        let pii_action = match &mut self.state.pii_stage {
            Some(stage) => stage.process_chunk(&json_str),
            None => StreamAction::Passthrough,
        };
        let final_json = match &pii_action {
            StreamAction::Mutate(s) => s.as_str(),
            _ => &json_str,
        };

        let sse_frame = crate::sse::build_sse_frame(final_json);
        if let Err(e) = self.send_to_sink(ctx, sse_frame).await {
            let fail_ctx = self.state.make_failure_context(ctx);
            return Ok(crate::streaming::ChunkEvent::Return(Box::new(
                self.dispatcher.fail_on_sink_send_error(e, fail_ctx),
            )));
        }

        // If normalizer stage has residual buffered content, flush before [DONE]
        if let Some(residual) = self.state.normalizer.finalize() {
            let sse_bytes = crate::sse::build_sse_frame(&residual);
            let _ = self.send_to_sink(ctx, sse_bytes).await;
        }

        // If PII stage has residual buffered content, flush before [DONE]
        if let Some(residual) = self.state.pii_stage.as_mut().and_then(|s| s.finalize()) {
            let sse_bytes = crate::sse::build_sse_frame(&residual);
            let _ = self.send_to_sink(ctx, sse_bytes).await;
        }

        if let Err(crate::race_sink::StreamSinkError::Lost) = self
            .send_to_sink(ctx, bytes::Bytes::clone(&SSE_DONE_BYTES))
            .await
        {
            let fail_ctx = self.state.make_failure_context(ctx);
            return Ok(crate::streaming::ChunkEvent::Return(Box::new(
                self.dispatcher
                    .fail_on_sink_send_error(crate::race_sink::StreamSinkError::Lost, fail_ctx),
            )));
        }
        self.state.done_sent = true;
        Ok(crate::streaming::ChunkEvent::Done)
    }

    pub(super) async fn handle_translated_chunk(
        &mut self,
        ctx: &StreamContext<'_>,
        stream: &mut openproxy_adapters::upstream::UpstreamBodyStream,
        mut chunk: crate::sse::UpstreamSseChunk,
    ) -> Result<crate::streaming::ChunkEvent, CoreError> {
        if let Some(new_usage) = chunk.usage.take() {
            self.state.usage = Some(match self.state.usage.take() {
                Some(existing) => merge_usage(existing, new_usage),
                None => new_usage,
            });
        }
        if chunk.stop_reason.is_some() && self.state.stop_reason.is_none() {
            self.state.stop_reason = chunk.stop_reason.take();
        }
        let delta_reasoning = chunk.delta_reasoning.take();
        let _delta_tool_calls = std::mem::take(&mut chunk.delta_tool_calls);
        let chunk_has_content = chunk.has_content;
        let json_str = chunk.into_json_string();

        let norm_action = self.state.normalizer.process_chunk(&json_str);
        if matches!(norm_action, StreamAction::Skip) {
            if chunk_has_content {
                stream.note_content_chunk();
            }
            return Ok(crate::streaming::ChunkEvent::Skip);
        }
        let normalized_json = match &norm_action {
            StreamAction::Mutate(s) => s.as_str(),
            _ => &json_str,
        };

        if let Some(a) = self.state.acc.as_mut() {
            if let Some(u) = &self.state.usage {
                a.set_usage(u.to_owned());
            }
            if let Some(sr) = &self.state.stop_reason {
                a.set_stop_reason(sr);
            }
            if let Some(dr) = &delta_reasoning
                && !dr.is_empty()
            {
                a.append_reasoning(dr);
            }
            a.append_openai_raw(normalized_json);
        }

        let pii_action = match &mut self.state.pii_stage {
            Some(stage) => stage.process_chunk(normalized_json),
            None => StreamAction::Passthrough,
        };
        let final_json = match &pii_action {
            StreamAction::Mutate(s) => s.as_str(),
            _ => normalized_json,
        };

        let sse_frame = crate::sse::build_sse_frame(final_json);
        if chunk_has_content {
            stream.note_content_chunk();
        }
        if let Err(e) = self.send_to_sink(ctx, sse_frame).await {
            let fail_ctx = self.state.make_failure_context(ctx);
            return Ok(crate::streaming::ChunkEvent::Return(Box::new(
                self.dispatcher.fail_on_sink_send_error(e, fail_ctx),
            )));
        }
        Ok(crate::streaming::ChunkEvent::Skip)
    }

    /// Gemini / Anthropic SSE handler — translates to OpenAI shape, then forwards.
    pub(super) async fn process_translated_format(
        &mut self,
        ctx: &StreamContext<'_>,
        stream: &mut openproxy_adapters::upstream::UpstreamBodyStream,
        line: &str,
    ) -> Result<crate::streaming::ChunkEvent, CoreError> {
        let line_payload = line
            .strip_prefix("data: ")
            .or_else(|| line.strip_prefix("data:"))
            .unwrap_or(line)
            .trim();
        if let Some(ret) = self.check_and_handle_inline_upstream_error(ctx, line_payload) {
            return Ok(ret);
        }

        let parsed = parse_translated_sse_line(
            self.state,
            ctx.target_format,
            line,
            ctx.chunk_id,
            ctx.created,
            ctx.model_name,
        );

        match parsed {
            Ok(Some(chunk)) => {
                if chunk.done {
                    self.handle_translated_done(ctx, chunk).await
                } else {
                    self.handle_translated_chunk(ctx, stream, chunk).await
                }
            }
            Ok(None) => Ok(crate::streaming::ChunkEvent::Skip),
            Err(e) => {
                tracing::warn!(
                    chunk_id = %ctx.chunk_id,
                    error = %e,
                    "failed to parse SSE line from upstream"
                );
                let acc_ref = self.state.acc.as_mut().map(|a| {
                    a.mark_partial();
                    &*a
                });
                Ok(crate::streaming::ChunkEvent::Return(Box::new(
                    self.dispatcher.record_and_fail_with_trace_id_and_partial(
                        crate::PartialFailureParams {
                            req: ctx.req.to_owned(),
                            combo: ctx.combo,
                            target: ctx.target,
                            ctx: crate::FailureContext {
                                proxy_url: ctx.proxy_url.clone(),
                                proxy_status: ctx.proxy_status.clone(),
                                attempt: ctx.attempt,
                                race_size: ctx.race_size,
                                err: &e,
                                started: ctx.started,
                                model: Some(ctx.model),
                                connect_ms: Some(ctx.connect_and_send_ms),
                                ttft_ms: self.state.ttft_ms,
                                status_code: e.http_status(),
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
        }
    }
}
