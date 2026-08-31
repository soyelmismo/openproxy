use crate::FailureContext;
use crate::race_sink::StreamSink;
use crate::sse::AnthropicToolUseAccumulator;
use crate::sse::SseParser;
use crate::sse_accumulator::ResponseAccumulator;
use crate::streaming::{StreamAction, StreamingChunkStage};
use crate::think_extractor::ThinkStreamExtractor;
use crate::{PipelineRequest, PipelineResult, SSE_DONE_BYTES};
use openproxy_types::combos::{Combo, ComboTarget};
use openproxy_types::error::CoreError;
use openproxy_types::models::Model;
use std::time::Instant;

use crate::translation::OpenAIUsage;

/// Maximum allowed length for accumulated tool call arguments string.
/// Prevents unbounded memory growth from malicious or buggy upstream.
const MAX_TOOL_CALL_ARGS_BYTES: usize = 1_048_576; // 1 MiB

#[derive(Default)]
pub struct ToolCallAccumulator {
    /// Map of tool_call index → running total of arguments seen so far.
    args_by_index: std::collections::HashMap<u64, String>,
}

fn extract_argument_fragment<'a>(prev: &str, arguments: &'a str) -> &'a str {
    if prev.is_empty() {
        arguments
    } else {
        arguments.strip_prefix(prev).unwrap_or(arguments)
    }
}

impl ToolCallAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process a tool_call delta. Returns the `arguments` value that
    /// should be sent to the client (just the new fragment, not the
    /// running total). If the upstream already sends fragments (the
    /// correct behavior), this is a no-op — the fragment is returned
    /// as-is and the running total is updated.
    pub fn process<'a>(&mut self, index: u64, arguments: &'a str) -> &'a str {
        let prev = self.args_by_index.entry(index).or_default();
        let fragment = extract_argument_fragment(prev, arguments);

        if prev.len() + fragment.len() > MAX_TOOL_CALL_ARGS_BYTES {
            return "";
        }
        prev.push_str(fragment);
        fragment
    }
}

fn normalize_choice_content(
    delta: &mut FastDelta<'_>,
    think_extractor: &mut crate::think_extractor::ThinkStreamExtractor,
) -> bool {
    let Some(content) = delta.content.as_ref() else {
        return false;
    };
    let mut modified = false;
    let (clean_content, extracted_reasoning) = think_extractor.process(content);
    if clean_content != *content {
        delta.content = Some(std::borrow::Cow::Owned(clean_content));
        modified = true;
    }

    let has_native_reasoning =
        delta.reasoning_content.is_some() || delta.extra.contains_key("reasoning_content");
    if !extracted_reasoning.is_empty() && !has_native_reasoning {
        delta.reasoning_content = Some(std::borrow::Cow::Owned(extracted_reasoning));
        modified = true;
    }
    modified
}

fn normalize_choice_tool_calls(
    delta: &mut FastDelta<'_>,
    tool_call_acc: &mut ToolCallAccumulator,
) -> bool {
    let Some(tool_calls) = &mut delta.tool_calls else {
        return false;
    };
    let mut modified = false;
    for tc in tool_calls {
        if let Some(func) = &mut tc.function
            && let Some(arguments) = func.arguments.as_ref()
        {
            let index = tc.index.unwrap_or(0);
            let new_fragment = tool_call_acc.process(index, arguments);
            if new_fragment != *arguments {
                func.arguments = Some(std::borrow::Cow::Owned(new_fragment.to_string()));
                modified = true;
            }
        }
    }
    modified
}

#[derive(serde::Deserialize, serde::Serialize)]
struct FastChunk<'a> {
    #[serde(borrow, skip_serializing_if = "Option::is_none")]
    choices: Option<Vec<FastChoice<'a>>>,
    #[serde(flatten, borrow)]
    extra: std::collections::HashMap<&'a str, &'a serde_json::value::RawValue>,
}
#[derive(serde::Deserialize, serde::Serialize)]
struct FastChoice<'a> {
    #[serde(borrow, skip_serializing_if = "Option::is_none")]
    delta: Option<FastDelta<'a>>,
    #[serde(flatten, borrow)]
    extra: std::collections::HashMap<&'a str, &'a serde_json::value::RawValue>,
}
#[derive(serde::Deserialize, serde::Serialize)]
struct FastDelta<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<std::borrow::Cow<'a, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow, skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<FastToolCall<'a>>>,
    #[serde(flatten, borrow)]
    extra: std::collections::HashMap<&'a str, &'a serde_json::value::RawValue>,
}
#[derive(serde::Deserialize, serde::Serialize)]
struct FastToolCall<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    index: Option<u64>,
    #[serde(borrow, skip_serializing_if = "Option::is_none")]
    function: Option<FastFunction<'a>>,
    #[serde(flatten, borrow)]
    extra: std::collections::HashMap<&'a str, &'a serde_json::value::RawValue>,
}
#[derive(serde::Deserialize, serde::Serialize)]
struct FastFunction<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<std::borrow::Cow<'a, str>>,
    #[serde(flatten, borrow)]
    extra: std::collections::HashMap<&'a str, &'a serde_json::value::RawValue>,
}

pub(crate) fn apply_reasoning_normalizations(
    payload: &str,
    think_extractor: &mut crate::think_extractor::ThinkStreamExtractor,
    tool_call_acc: &mut ToolCallAccumulator,
) -> Option<String> {
    // Step 1: normalize non-standard reasoning fields.
    let normalized = crate::sse_accumulator::normalize_nonstandard_reasoning_fields(payload);
    let p: &str = normalized.as_deref().unwrap_or(payload);

    // Fast check: if there's no "content" AND no "tool_calls", skip
    // the JSON parse entirely — the chunk is role-only, finish, etc.
    let has_content = p.contains("\"content\"");
    let has_tool_calls = p.contains("\"tool_calls\"");
    if !has_content && !has_tool_calls {
        return normalized;
    }

    if let Ok(mut fc) = serde_json::from_str::<FastChunk>(p) {
        let mut modified = false;

        if let Some(choices) = &mut fc.choices
            && let Some(choice) = choices.first_mut()
            && let Some(delta) = &mut choice.delta
        {
            let c_mod = normalize_choice_content(delta, think_extractor);
            let t_mod = normalize_choice_tool_calls(delta, tool_call_acc);
            modified = c_mod || t_mod;
        }

        if modified {
            return serde_json::to_string(&fc).ok().or(normalized);
        }
    }

    normalized
}

/// Modular streaming stage that normalizes non-standard reasoning fields,
/// extracts `<think>` blocks into `reasoning_content`, and normalizes tool call arguments.
#[derive(Default)]
pub struct ReasoningNormalizer {
    pub think_extractor: ThinkStreamExtractor,
    pub tool_call_acc: ToolCallAccumulator,
}

impl ReasoningNormalizer {
    pub fn new() -> Self {
        Self::default()
    }
}

impl StreamingChunkStage for ReasoningNormalizer {
    fn process_chunk(&mut self, payload: &str) -> StreamAction {
        match apply_reasoning_normalizations(
            payload,
            &mut self.think_extractor,
            &mut self.tool_call_acc,
        ) {
            Some(modified) => StreamAction::Mutate(modified),
            None => StreamAction::Passthrough,
        }
    }

    fn finalize(&mut self) -> Option<String> {
        let (clean_content, _) = self.think_extractor.flush();
        if clean_content.is_empty() {
            None
        } else {
            Some(clean_content)
        }
    }
}

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

pub(crate) struct ChunkProcessor<'a> {
    pub state: &'a mut StreamingState,
    pub dispatcher: &'a crate::upstream_dispatcher::UpstreamDispatcher,
}
impl crate::streaming::ChunkInterceptor for ChunkProcessor<'_> {
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

fn decode_and_record_line<'a>(state: &mut StreamingState, line_bytes: &'a [u8]) -> Option<&'a str> {
    let line = std::str::from_utf8(line_bytes).ok()?.trim_end_matches('\r');
    if line.is_empty() || line.starts_with(':') {
        return None;
    }
    if let Some(a) = state.acc.as_mut() {
        a.append_raw_line(line);
    }
    Some(line)
}

fn record_ttft_if_first(ctx: &StreamContext<'_>, state: &mut StreamingState) {
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

fn parse_inline_error<'a>(
    json_payload: &'a str,
    default_provider: &'a str,
) -> Option<(u16, &'a str, &'a str)> {
    if !json_payload.contains("\"error\":")
        || (json_payload.contains("\"choices\":") && !json_payload.contains("\"choices\":[]"))
    {
        return None;
    }

    #[derive(serde::Deserialize)]
    struct ErrorChunk<'a> {
        #[serde(borrow)]
        choices: Option<Vec<&'a serde_json::value::RawValue>>,
        #[serde(borrow)]
        error: Option<ErrorObj<'a>>,
        #[serde(borrow)]
        provider: Option<&'a str>,
    }
    #[derive(serde::Deserialize)]
    struct ErrorObj<'a> {
        code: Option<u64>,
        #[serde(borrow)]
        message: Option<&'a str>,
    }

    let ec = serde_json::from_str::<ErrorChunk>(json_payload).ok()?;
    if !ec.choices.as_ref().is_none_or(std::vec::Vec::is_empty) {
        return None;
    }
    let error_obj = ec.error?;
    let code = error_obj.code.unwrap_or(502) as u16;
    let message = error_obj
        .message
        .unwrap_or("unknown upstream error in SSE stream");
    let provider = ec.provider.unwrap_or(default_provider);
    Some((code, message, provider))
}

fn parse_translated_sse_line(
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
        openproxy_types::TargetFormat::Atomesus => {
            crate::sse::parse_atomesus_sse_line(line, chunk_id, created, model_name)
        }
        openproxy_types::TargetFormat::Fx => {
            crate::sse::parse_fx_sse_line(line, chunk_id, created, model_name)
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
    }
}

// ── Format-specific handlers ──
//
// Each method handles a single upstream SSE format. `process_openai_format`
// includes both the fast path (pure content delta — zero JSON parsing) and
// the slow path (state.usage / finish_reason / inline errors).
// `process_translated_format` handles Gemini and Anthropic by first
// translating to OpenAI shape and then forwarding.
impl ChunkProcessor<'_> {
    fn check_race_cancelled(
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

    async fn handle_done_sentinel(
        &mut self,
        ctx: &StreamContext<'_>,
    ) -> Result<crate::streaming::ChunkEvent, CoreError> {
        if let Some(event) = self.check_race_cancelled(ctx) {
            return Ok(event);
        }
        if let Err(crate::race_sink::StreamSinkError::Lost) =
            ctx.sink.send(bytes::Bytes::clone(&SSE_DONE_BYTES)).await
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

    fn check_and_handle_inline_upstream_error(
        &mut self,
        ctx: &StreamContext<'_>,
        json_payload: &str,
    ) -> Option<crate::streaming::ChunkEvent> {
        let (code, message, provider_name) =
            parse_inline_error(json_payload, ctx.target.provider_id.as_str())?;

        tracing::warn!(
            combo_id = ctx.combo.id.0,
            target_id = ctx.target.id.0,
            provider = %provider_name,
            code,
            message,
            "upstream error embedded in streaming chunk"
        );
        let err = CoreError::upstream_error(code, provider_name, ctx.model_name, message, false);
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

    fn update_state_and_acc_from_metadata_chunk(
        &mut self,
        mut chunk: crate::sse::UpstreamSseChunk,
        json_payload: &str,
    ) {
        if chunk.usage.is_some() {
            self.state.usage = chunk.usage.take();
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

    async fn process_openai_metadata_chunk(
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
        let sse_bytes = crate::sse::build_sse_frame(payload_str);

        if let Err(e) = ctx.sink.send(sse_bytes).await {
            let fail_ctx = self.state.make_failure_context(ctx);
            return Ok(crate::streaming::ChunkEvent::Return(Box::new(
                self.dispatcher.fail_on_sink_send_error(e, fail_ctx),
            )));
        }

        Ok(crate::streaming::ChunkEvent::Skip)
    }

    fn prepare_content_chunk_bytes(
        &mut self,
        json_payload: &str,
        line_bytes: &[u8],
    ) -> bytes::Bytes {
        let effective_payload = match self.state.normalizer.process_chunk(json_payload) {
            StreamAction::Mutate(s) => Some(s),
            _ => None,
        };

        if let Some(a) = self.state.acc.as_mut() {
            let payload = effective_payload.as_deref().unwrap_or(json_payload);
            a.process_chunk(payload);
        }

        match effective_payload {
            Some(modified) => crate::sse::build_sse_frame(&modified),
            None => {
                let mut frame = bytes::BytesMut::from(line_bytes);
                frame.extend_from_slice(b"\n\n");
                frame.freeze()
            }
        }
    }

    async fn process_openai_content_chunk(
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
        if let Err(e) = ctx.sink.send(sse_bytes).await {
            let fail_ctx = self.state.make_failure_context(ctx);
            return Ok(crate::streaming::ChunkEvent::Return(Box::new(
                self.dispatcher.fail_on_sink_send_error(e, fail_ctx),
            )));
        }

        Ok(crate::streaming::ChunkEvent::Skip)
    }

    /// OpenAI-format SSE handler (fast + slow paths).
    async fn process_openai_format(
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

    async fn handle_translated_done(
        &mut self,
        ctx: &StreamContext<'_>,
        mut chunk: crate::sse::UpstreamSseChunk,
    ) -> Result<crate::streaming::ChunkEvent, CoreError> {
        if chunk.usage.is_some() {
            self.state.usage = chunk.usage.take();
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

        let sse_frame = crate::sse::build_sse_frame(&json_str);
        if let Err(e) = ctx.sink.send(sse_frame).await {
            let fail_ctx = self.state.make_failure_context(ctx);
            return Ok(crate::streaming::ChunkEvent::Return(Box::new(
                self.dispatcher.fail_on_sink_send_error(e, fail_ctx),
            )));
        }

        if let Err(crate::race_sink::StreamSinkError::Lost) =
            ctx.sink.send(bytes::Bytes::clone(&SSE_DONE_BYTES)).await
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

    async fn handle_translated_chunk(
        &mut self,
        ctx: &StreamContext<'_>,
        stream: &mut openproxy_adapters::upstream::UpstreamBodyStream,
        mut chunk: crate::sse::UpstreamSseChunk,
    ) -> Result<crate::streaming::ChunkEvent, CoreError> {
        if chunk.usage.is_some() {
            self.state.usage = chunk.usage.take();
        }
        if chunk.stop_reason.is_some() && self.state.stop_reason.is_none() {
            self.state.stop_reason = chunk.stop_reason.take();
        }
        let delta_reasoning = chunk.delta_reasoning.take();
        let _delta_tool_calls = std::mem::take(&mut chunk.delta_tool_calls);
        let chunk_has_content = chunk.has_content;
        let json_str = chunk.into_json_string();

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
            a.append_openai_raw(&json_str);
        }

        let sse_frame = crate::sse::build_sse_frame(&json_str);
        if chunk_has_content {
            stream.note_content_chunk();
        }
        if let Err(e) = ctx.sink.send(sse_frame).await {
            let fail_ctx = self.state.make_failure_context(ctx);
            return Ok(crate::streaming::ChunkEvent::Return(Box::new(
                self.dispatcher.fail_on_sink_send_error(e, fail_ctx),
            )));
        }
        Ok(crate::streaming::ChunkEvent::Skip)
    }

    /// Gemini / Anthropic SSE handler — translates to OpenAI shape, then forwards.
    async fn process_translated_format(
        &mut self,
        ctx: &StreamContext<'_>,
        stream: &mut openproxy_adapters::upstream::UpstreamBodyStream,
        line: &str,
    ) -> Result<crate::streaming::ChunkEvent, CoreError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reasoning_normalizer_stage_mutates_think_tags() {
        let mut normalizer = ReasoningNormalizer::new();
        let payload = r#"{"choices":[{"delta":{"content":"<think>reasoning</think>answer"}}]}"#;
        let action = normalizer.process_chunk(payload);
        let StreamAction::Mutate(mutated) = action else {
            panic!("expected StreamAction::Mutate, got {action:?}");
        };
        assert!(mutated.contains("\"reasoning_content\":\"reasoning\""));
        assert!(mutated.contains("\"content\":\"answer\""));
    }

    #[test]
    fn test_reasoning_normalizer_stage_passthrough_clean_chunk() {
        let mut normalizer = ReasoningNormalizer::new();
        let payload = r#"{"choices":[{"delta":{"content":"clean chunk"}}]}"#;
        let action = normalizer.process_chunk(payload);
        assert_eq!(action, StreamAction::Passthrough);
    }

    #[test]
    fn test_tool_call_accumulator_handles_fragments() {
        let mut acc = ToolCallAccumulator::new();
        let f1 = acc.process(0, "{\"location\":");
        assert_eq!(f1, "{\"location\":");
        let f2 = acc.process(0, " \"Paris\"}");
        assert_eq!(f2, " \"Paris\"}");
    }
}
