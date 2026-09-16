mod normalizer;
mod openai;
mod processor;
#[cfg(test)]
mod tests;
mod translated;

pub use normalizer::{ReasoningNormalizer, ToolCallAccumulator};
pub(crate) use processor::ChunkProcessor;

use crate::race_sink::StreamSink;
use crate::sse::AnthropicToolUseAccumulator;
use crate::sse::SseParser;
use crate::sse_accumulator::ResponseAccumulator;
use crate::translation::OpenAIUsage;
use crate::{PipelineRequest, PipelineResult};
use openproxy_types::combos::{Combo, ComboTarget};
use openproxy_types::error::CoreError;
use openproxy_types::models::Model;
use std::time::Instant;

pub(crate) struct StreamingState {
    pub sse_parser: SseParser,
    pub usage: Option<OpenAIUsage>,
    pub ttft_ms: Option<u64>,
    pub stop_reason: Option<String>,
    pub first_chunk_time: Instant,
    pub normalizer: ReasoningNormalizer,
    pub tool_use_acc: Option<AnthropicToolUseAccumulator>,
    pub tool_call_index_counter: u32,
    pub current_event_type: Option<String>,
    pub done_sent: bool,
    pub acc: Option<ResponseAccumulator>,
    pub responses_sse_state: crate::sse::ResponsesSseState,
    pub commandcode_sse_state: crate::sse::CommandCodeSseState,
    pub pii_stage: Option<crate::pii::PiiRestorationStage>,
}

pub(crate) struct StreamContext<'a> {
    pub req: &'a PipelineRequest,
    pub combo: &'a Combo,
    pub target: &'a ComboTarget,
    pub model: &'a Model,
    pub target_format: openproxy_types::TargetFormat,
    pub sink: &'a StreamSink,
    pub trace_id: &'a str,
    pub chunk_id: &'a str,
    pub model_name: &'a str,
    pub started: Instant,
    pub attempt: u8,
    pub race_size: u8,
    pub created: u64,
    pub connect_and_send_ms: u64,
    pub resolved_timeouts: &'a crate::timeouts::Timeouts,
    pub proxy_url: Option<String>,
    pub proxy_status: Option<String>,
}

pub(crate) enum ChunkResult {
    Break,
    Return(Box<PipelineResult>),
}

impl StreamingState {
    pub fn new(needs_accumulator: bool) -> Self {
        Self {
            sse_parser: SseParser::new(crate::sse::MAX_SSE_LINE_BYTES),
            usage: None,
            ttft_ms: None,
            stop_reason: None,
            first_chunk_time: Instant::now(),
            normalizer: ReasoningNormalizer::new(),
            tool_use_acc: None,
            tool_call_index_counter: 0,
            current_event_type: None,
            done_sent: false,
            acc: if needs_accumulator {
                Some(ResponseAccumulator::new())
            } else {
                None
            },
            responses_sse_state: crate::sse::ResponsesSseState::default(),
            commandcode_sse_state: crate::sse::CommandCodeSseState::default(),
            pii_stage: None,
        }
    }

    pub(crate) async fn run_stream_loop(
        &mut self,
        ctx: &StreamContext<'_>,
        dispatcher: &crate::upstream_dispatcher::UpstreamDispatcher,
        stream: &mut openproxy_adapters::upstream::UpstreamBodyStream,
    ) -> Result<ChunkResult, CoreError> {
        let sse_parser = std::mem::replace(&mut self.sse_parser, crate::sse::SseParser::new(0));
        let mut processor = ChunkProcessor {
            state: self,
            dispatcher,
        };
        let pipeline_result =
            crate::streaming::pipeline::run_pipeline(ctx, stream, sse_parser, &mut processor)
                .await?;
        if let ChunkResult::Return(_) = pipeline_result {
            return Ok(pipeline_result);
        }

        // Cancellation checkpoint
        if ctx
            .req
            .race_cancel
            .as_ref()
            .is_some_and(openproxy_adapters::CancellationToken::is_cancelled)
            && !processor.state.done_sent
        {
            let fail_ctx = processor.state.make_failure_context(ctx);
            return Ok(ChunkResult::Return(Box::new(
                dispatcher.fail_stream_client_disconnected(fail_ctx),
            )));
        }
        Ok(ChunkResult::Break)
    }

    pub(crate) fn make_failure_context<'c>(
        &'c mut self,
        ctx: &'c StreamContext<'_>,
    ) -> crate::upstream_dispatcher::StreamFailureContext<'c> {
        crate::upstream_dispatcher::StreamFailureContext {
            proxy_url: ctx.proxy_url.clone(),
            proxy_status: ctx.proxy_status.clone(),
            req: ctx.req.to_owned(),
            combo: ctx.combo,
            target: ctx.target,
            attempt: ctx.attempt,
            race_size: ctx.race_size,
            started: ctx.started,
            model: ctx.model,
            connect_ms: ctx.connect_and_send_ms,
            ttft_ms: self.ttft_ms,
            trace_id: ctx.trace_id.to_string(),
            acc: self.acc.as_mut(),
            chunk_id: ctx.chunk_id,
            created: ctx.created,
            model_name: ctx.model_name,
        }
    }
}
