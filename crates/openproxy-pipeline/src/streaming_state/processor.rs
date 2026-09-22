use super::{StreamContext, StreamingState};
use crate::SSE_DONE_BYTES;
use crate::streaming::ChunkInterceptor;
use openproxy_types::error::CoreError;

pub(crate) struct ChunkProcessor<'a> {
    pub state: &'a mut StreamingState,
    pub dispatcher: &'a crate::upstream_dispatcher::UpstreamDispatcher,
}

impl ChunkInterceptor for ChunkProcessor<'_> {
    async fn process_chunk(
        &mut self,
        ctx: &StreamContext<'_>,
        stream: &mut openproxy_adapters::upstream::UpstreamBodyStream,
        event: crate::streaming::ChunkEvent,
    ) -> Result<crate::streaming::ChunkEvent, CoreError> {
        let crate::streaming::ChunkEvent::Data(line_bytes) = event else {
            return Ok(event);
        };
        if self.state.done_sent {
            return Ok(crate::streaming::ChunkEvent::Done);
        }

        let Some(line) = decode_and_record_line(self.state, &line_bytes) else {
            return Ok(crate::streaming::ChunkEvent::Skip);
        };

        record_ttft_if_first(ctx, self.state);

        if let Some(cancel_event) = self.check_race_cancelled(ctx) {
            return Ok(cancel_event);
        }

        if ctx.target_format == openproxy_types::TargetFormat::Openai {
            self.process_openai_format(ctx, stream, line, &line_bytes)
                .await
        } else {
            self.process_translated_format(ctx, stream, line).await
        }
    }
}

pub(super) fn decode_and_record_line<'a>(
    state: &mut StreamingState,
    line_bytes: &'a [u8],
) -> Option<&'a str> {
    let line = std::str::from_utf8(line_bytes).ok()?.trim_end_matches('\r');
    if line.is_empty() {
        state.current_event_type = None;
        return None;
    }
    if line.starts_with(':') {
        return None;
    }
    if let Some(a) = state.acc.as_mut() {
        a.append_raw_line(line);
    }
    Some(line)
}

pub(super) fn record_ttft_if_first(ctx: &StreamContext<'_>, state: &mut StreamingState) {
    if state.ttft_ms.is_none() {
        state.ttft_ms = Some(state.first_chunk_time.elapsed().as_millis() as u64);
        openproxy_types::emit_stage_event!(
            request_id: ctx.req.request_id,
            trace_id: ctx.trace_id,
            stage: "streaming",
            elapsed_ms: ctx.started.elapsed().as_millis() as u64,
            connect_ms: ctx.connect_and_send_ms,
            ttft_ms: state.ttft_ms,
            status_code: 200,
        );
    }
}

impl ChunkProcessor<'_> {
    pub(super) fn check_race_cancelled(
        &mut self,
        ctx: &StreamContext<'_>,
    ) -> Option<crate::streaming::ChunkEvent> {
        if ctx
            .req
            .race_cancel
            .as_ref()
            .is_some_and(openproxy_adapters::CancellationToken::is_cancelled)
        {
            let fail_ctx = self.state.make_failure_context(ctx);
            Some(crate::streaming::ChunkEvent::Return(Box::new(
                self.dispatcher.fail_stream_client_disconnected(fail_ctx),
            )))
        } else {
            None
        }
    }

    pub(super) async fn send_to_sink(
        &mut self,
        ctx: &StreamContext<'_>,
        chunk: bytes::Bytes,
    ) -> Result<(), crate::race_sink::StreamSinkError> {
        if chunk.is_empty() {
            return Ok(());
        }
        ctx.sink.send(chunk).await
    }

    pub(super) async fn handle_done_sentinel(
        &mut self,
        ctx: &StreamContext<'_>,
    ) -> Result<crate::streaming::ChunkEvent, CoreError> {
        if let Some(event) = self.check_race_cancelled(ctx) {
            return Ok(event);
        }
        // If normalizer stage has residual buffered content or tool call, flush before [DONE]
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
}
