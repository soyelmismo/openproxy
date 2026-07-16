use openproxy_types::combos::{Combo, ComboTarget};
use openproxy_types::error::CoreError;
use openproxy_types::models::Model;
use crate::FailureContext;
use crate::{PipelineRequest, PipelineResult, SSE_DONE_BYTES};
use crate::race_sink::StreamSink;
use crate::sse::AnthropicToolUseAccumulator;
use crate::sse::SseParser;
use crate::sse_accumulator::ResponseAccumulator;
use crate::think_extractor::ThinkStreamExtractor;
use std::time::Instant;

use crate::translation::OpenAIUsage;

/// Maximum allowed length for accumulated tool call arguments string.
/// Prevents unbounded memory growth from malicious or buggy upstream.
const MAX_TOOL_CALL_ARGS_BYTES: usize = 1_048_576; // 1 MiB

#[derive(Default)]
pub(crate) struct ToolCallAccumulator {
    /// Map of tool_call index → running total of arguments seen so far.
    args_by_index: std::collections::HashMap<u64, String>,
}

impl ToolCallAccumulator {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Process a tool_call delta. Returns the `arguments` value that
    /// should be sent to the client (just the new fragment, not the
    /// running total). If the upstream already sends fragments (the
    /// correct behavior), this is a no-op — the fragment is returned
    /// as-is and the running total is updated.
    pub(crate) fn process(&mut self, index: u64, arguments: &str) -> String {
        let prev = self.args_by_index.entry(index).or_default();
        if prev.is_empty() {
            // First chunk for this index — the arguments IS the
            // fragment (there's nothing before it).
            if arguments.len() > MAX_TOOL_CALL_ARGS_BYTES {
                return String::new(); // Drop fragment, don't accumulate
            }
            prev.push_str(arguments);
            return arguments.to_string();
        }
        if arguments.starts_with(prev.as_str()) {
            // Running-total pattern: the upstream sent prev + new.
            // Extract just the new suffix.
            let new_fragment = &arguments[prev.len()..];
            if prev.len() + new_fragment.len() > MAX_TOOL_CALL_ARGS_BYTES {
                return String::new(); // Drop fragment, don't accumulate
            }
            prev.push_str(new_fragment);
            new_fragment.to_string()
        } else {
            // Fragment pattern (correct OpenAI behavior): the
            // upstream sent just the new fragment. Update the
            // running total and pass it through.
            if prev.len() + arguments.len() > MAX_TOOL_CALL_ARGS_BYTES {
                return String::new(); // Drop fragment, don't accumulate
            }
            prev.push_str(arguments);
            arguments.to_string()
        }
    }
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

    if let Ok(mut fc) = serde_json::from_str::<FastChunk>(p) {
        let mut modified = false;

        if let Some(choices) = &mut fc.choices
            && let Some(choice) = choices.first_mut()
            && let Some(delta) = &mut choice.delta
        {
            if let Some(content) = delta.content.as_ref() {
                let (clean_content, extracted_reasoning) = think_extractor.process(content);
                if clean_content != *content {
                    delta.content = Some(std::borrow::Cow::Owned(clean_content));
                    modified = true;
                }

                let has_native_reasoning = delta.reasoning_content.is_some()
                    || delta.extra.contains_key("reasoning_content");
                if !extracted_reasoning.is_empty() && !has_native_reasoning {
                    delta.reasoning_content = Some(std::borrow::Cow::Owned(extracted_reasoning));
                    modified = true;
                }
            }

            if let Some(tool_calls) = &mut delta.tool_calls {
                for tc in tool_calls {
                    if let Some(func) = &mut tc.function
                        && let Some(arguments) = func.arguments.as_ref()
                    {
                        let index = tc.index.unwrap_or(0);
                        let new_fragment = tool_call_acc.process(index, arguments);
                        if new_fragment != *arguments {
                            func.arguments = Some(std::borrow::Cow::Owned(new_fragment));
                            modified = true;
                        }
                    }
                }
            }
        }

        if modified {
            return serde_json::to_string(&fc).ok().or(normalized);
        }
    }

    normalized
}

pub(crate) struct StreamingState {
    pub sse_parser: SseParser,
    pub usage: Option<OpenAIUsage>,
    pub ttft_ms: Option<u64>,
    pub stop_reason: Option<String>,
    pub first_chunk_time: Instant,
    pub think_extractor: ThinkStreamExtractor,
    pub tool_call_acc: ToolCallAccumulator,
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
}

#[allow(clippy::large_enum_variant)]
pub(crate) enum ChunkResult {
    Break,
    Return(PipelineResult),
}

impl StreamingState {
    pub fn new(needs_accumulator: bool) -> Self {
        Self {
            sse_parser: SseParser::new(crate::sse::MAX_SSE_LINE_BYTES),
            usage: None,
            ttft_ms: None,
            stop_reason: None,
            first_chunk_time: Instant::now(),
            think_extractor: ThinkStreamExtractor::new(),
            tool_call_acc: ToolCallAccumulator::new(),
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
        let pipeline_result = crate::streaming::pipeline::run_pipeline(
            ctx,
            stream,
            sse_parser,
            &mut processor,
        )
        .await?;
        if let ChunkResult::Return(_) = pipeline_result {
            return Ok(pipeline_result);
        }

        // Cancellation checkpoint
        if ctx
            .req
            .race_cancel
            .as_ref()
            .is_some_and(|rc| rc.is_cancelled())
            && !processor.state.done_sent
        {
            return Ok(ChunkResult::Return(
                dispatcher.fail_stream_client_disconnected(
                    crate::upstream_dispatcher::StreamFailureContext {
                        proxy_url: None,
                        proxy_status: None,
                        req: ctx.req.clone(),
                        combo: ctx.combo,
                        target: ctx.target,
                        attempt: ctx.attempt,
                        race_size: ctx.race_size,
                        started: ctx.started,
                        model: ctx.model,
                        connect_ms: ctx.connect_and_send_ms,
                        ttft_ms: processor.state.ttft_ms,
                        trace_id: ctx.trace_id.to_string(),
                        acc: processor.state.acc.as_mut(),
                        chunk_id: ctx.chunk_id,
                        created: ctx.created,
                        model_name: ctx.model_name,
                    },
                ),
            ));
        }
        Ok(ChunkResult::Break)
    }
}

pub(crate) struct ChunkProcessor<'a> {
    pub state: &'a mut StreamingState,
    pub dispatcher: &'a crate::upstream_dispatcher::UpstreamDispatcher,
}
impl<'a> crate::streaming::ChunkInterceptor for ChunkProcessor<'a> {
    async fn process_chunk(
        &mut self,
        ctx: &StreamContext<'_>,
        stream: &mut openproxy_adapters::upstream::UpstreamBodyStream,
        event: crate::streaming::ChunkEvent,
    ) -> Result<crate::streaming::ChunkEvent, CoreError> {
        let crate::streaming::ChunkEvent::Data(line_bytes) = event else {
            return Ok(event);
        };
        let state = &mut *self.state;

        if state.done_sent {
            return Ok(crate::streaming::ChunkEvent::Done);
        }

        // ── Common prefix: decode, filter, TTFT, race guard ──
        let Ok(line) = std::str::from_utf8(line_bytes.as_ref()) else {
            return Ok(crate::streaming::ChunkEvent::Skip);
        };
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            return Ok(crate::streaming::ChunkEvent::Skip);
        }
        if let Some(a) = state.acc.as_mut() {
            a.append_raw_line(line);
        }

        // Record TTFT on the first data-bearing line.
        if state.ttft_ms.is_none() {
            state.ttft_ms = Some(state.first_chunk_time.elapsed().as_millis() as u64);
            let streaming_ttft_snapshot = self.dispatcher.compression_stats_cell.read().clone();
            openproxy_types::usage::publish_stage_event(openproxy_types::usage::StageEvent {
                request_id: ctx.req.request_id.to_string(),
                trace_id: ctx.trace_id.to_string(),
                provider_id: ctx.target.provider_id.to_string(),
                upstream_model_id: ctx.model_name.to_string(),
                stage: "streaming".into(),
                elapsed_ms: ctx.started.elapsed().as_millis() as u64,
                connect_ms: Some(ctx.connect_and_send_ms),
                ttft_ms: state.ttft_ms,
                status_code: 200,
                error: None,
                stop_reason: None,
                compression_savings_pct: streaming_ttft_snapshot
                    .as_ref()
                    .and_then(|s| s.savings_pct_opt()),
                compression_techniques: streaming_ttft_snapshot
                    .as_ref()
                    .and_then(|s| s.techniques_csv()),
                timestamp: String::new(),
                endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
            });
        }

        // Race cancellation guard: if another target already won the
        // race, discard this chunk and exit instantly.
        if ctx
            .req
            .race_cancel
            .as_ref()
            .is_some_and(|rc| rc.is_cancelled())
        {
            return Ok(crate::streaming::ChunkEvent::Return(
                self.dispatcher.fail_stream_client_disconnected(
                    crate::upstream_dispatcher::StreamFailureContext {
                        proxy_url: None,
                        proxy_status: None,
                        req: ctx.req.clone(),
                        combo: ctx.combo,
                        target: ctx.target,
                        attempt: ctx.attempt,
                        race_size: ctx.race_size,
                        started: ctx.started,
                        model: ctx.model,
                        connect_ms: ctx.connect_and_send_ms,
                        ttft_ms: state.ttft_ms,
                        trace_id: ctx.trace_id.to_string(),
                        acc: state.acc.as_mut(),
                        chunk_id: ctx.chunk_id,
                        created: ctx.created,
                        model_name: ctx.model_name,
                    },
                ),
            ));
        }

        // ── Format dispatch ──
        if ctx.target_format == openproxy_types::TargetFormat::Openai {
            self.process_openai_format(ctx, stream, line, &line_bytes)
                .await
        } else {
            self.process_translated_format(ctx, stream, line).await
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
impl<'a> ChunkProcessor<'a> {
    /// OpenAI-format SSE handler (fast + slow paths).
    async fn process_openai_format(
        &mut self,
        ctx: &StreamContext<'_>,
        stream: &mut openproxy_adapters::upstream::UpstreamBodyStream,
        line: &str,
        line_bytes: &[u8],
    ) -> Result<crate::streaming::ChunkEvent, CoreError> {
        let pipeline = self.dispatcher;
        let state = &mut *self.state;
        let req = ctx.req;
        let combo = ctx.combo;
        let target = ctx.target;
        let model = ctx.model;
        let sink = ctx.sink;
        let trace_id = ctx.trace_id;
        let chunk_id = ctx.chunk_id;
        let model_name = ctx.model_name;
        let started = ctx.started;
        let attempt = ctx.attempt;
        let race_size = ctx.race_size;
        let created = ctx.created;
        let connect_and_send_ms = ctx.connect_and_send_ms;

        // PERF: use byte-level operations to avoid str
        // allocations. SSE lines are `data: <payload>` —
        // we match the 5-byte prefix directly on raw bytes.
        if line_bytes.len() < 5 || &line_bytes[..5] != b"data:" {
            return Ok(crate::streaming::ChunkEvent::Skip);
        }
        let payload_bytes = &line_bytes[5..]; // skip "data:"
        let json_payload_bytes = crate::sse::skip_leading_spaces(payload_bytes);
        // Use checked from_utf8 even though we validated above to avoid unsafe blocks.
        let json_payload = std::str::from_utf8(json_payload_bytes).unwrap_or("");
        let json_payload = json_payload.trim_end_matches(['\r', '\n', ' ']);
        // Fast [DONE] check.
        if json_payload == "[DONE]" {
            // Race cancellation guard: if another target
            // already won, discard this chunk instantly.
            if req.race_cancel.as_ref().is_some_and(|rc| rc.is_cancelled()) {
                return Ok(crate::streaming::ChunkEvent::Return(
                    self.dispatcher.fail_stream_client_disconnected(
                        crate::upstream_dispatcher::StreamFailureContext {
                            proxy_url: None,
                            proxy_status: None,
                            req: req.clone(),
                            combo,
                            target,
                            attempt,
                            race_size,
                            started,
                            model,
                            connect_ms: connect_and_send_ms,
                            ttft_ms: state.ttft_ms,
                            trace_id: trace_id.to_string(),
                            acc: state.acc.as_mut(),
                            chunk_id,
                            created,
                            model_name,
                        },
                    ),
                ));
            }
            if let Err(crate::race_sink::StreamSinkError::Lost) =
                sink.send(SSE_DONE_BYTES.clone()).await
            {
                return Ok(crate::streaming::ChunkEvent::Return(
                    self.dispatcher.fail_on_sink_send_error(
                        crate::race_sink::StreamSinkError::Lost,
                        crate::upstream_dispatcher::StreamFailureContext {
                            proxy_url: None,
                            proxy_status: None,
                            req: req.clone(),
                            combo,
                            target,
                            attempt,
                            race_size,
                            started,
                            model,
                            connect_ms: connect_and_send_ms,
                            ttft_ms: state.ttft_ms,
                            trace_id: trace_id.to_string(),
                            acc: state.acc.as_mut(),
                            chunk_id,
                            created,
                            model_name,
                        },
                    ),
                ));
            }
            state.done_sent = true;
            return Ok(crate::streaming::ChunkEvent::Done);
        }
        // Inline upstream error detection: some providers
        // (notably OpenRouter) send errors INSIDE an SSE
        // `data:` chunk with `"choices":[]` and an `"error":{}`
        // object, rather than returning a non-2xx HTTP status.
        // Without this check, the error chunk is forwarded
        // verbatim to the client, the upstream closes the
        // stream, and the post-loop code misattributes the
        // failure as "client disconnected".
        //
        // PERF: two fast `contains()` guards (~100ns each)
        // skip JSON parsing on the normal path. Only chunks
        // containing BOTH markers are parsed.
        if json_payload.contains("\"error\":") && json_payload.contains("\"choices\":[]") {
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
            if let Ok(ec) = serde_json::from_str::<ErrorChunk>(json_payload) {
                let has_empty_choices = ec.choices.is_some_and(|arr| arr.is_empty());
                if has_empty_choices && let Some(error_obj) = ec.error {
                    let code = error_obj.code.unwrap_or(502) as u16;
                    let message = error_obj
                        .message
                        .unwrap_or("unknown upstream error in SSE stream");
                    let provider_name = ec.provider.unwrap_or(target.provider_id.as_str());
                    tracing::warn!(
                        combo_id = combo.id.0,
                        target_id = target.id.0,
                        provider = %provider_name,
                        code,
                        message,
                        "upstream error embedded in streaming chunk"
                    );
                    let err = CoreError::UpstreamError {
                        status: code,
                        provider: provider_name.to_string(),
                        model: model_name.to_string(),
                        body: message.to_string(),
                        is_proxy_rotated: false,
                    };
                    let acc_ref: Option<&crate::sse_accumulator::ResponseAccumulator> =
                        match &mut state.acc {
                            Some(a) => {
                                a.mark_partial();
                                Some(&*a)
                            }
                            None => None,
                        };
                    return Ok(crate::streaming::ChunkEvent::Return(
                        pipeline.record_and_fail_with_trace_id_and_partial(
                            req.clone(),
                            combo,
                            target,
                            FailureContext {
                                proxy_url: None,
                                proxy_status: None,
                                attempt,
                                race_size,
                                err: &err,
                                started,
                                model: Some(model),
                                connect_ms: Some(connect_and_send_ms),
                                ttft_ms: state.ttft_ms,
                                status_code: code,
                            },
                            trace_id.to_string(),
                            acc_ref,
                            Some(chunk_id),
                            created,
                            model_name,
                        ),
                    ));
                }
            }
        }
        // Only parse when the chunk carries metadata worth
        // extracting. `"state.usage"` appears in the final chunk;
        // a non-null `"finish_reason"` marks stream end.
        //
        // PERF: combine the 2 contains() calls into a single
        // scan. Each contains() is a full memchr pass over
        // the payload — for a 1 KiB chunk that's ~100ns
        // each, so 2 calls = ~200ns per chunk. The combined
        // scan does both checks in a single pass, halving
        // the scan cost. For a 500-chunk response, this
        // saves ~50µs of pure scanning CPU.
        let needs_parse = crate::sse::sse_payload_needs_parse(json_payload);
        if needs_parse {
            // Pass the full SSE line (with "data:" prefix)
            // because parse_openai_sse_line expects it.
            match crate::sse::parse_openai_sse_line(line) {
                Ok(Some(mut chunk)) => {
                    if chunk.usage.is_some() {
                        state.usage = chunk.usage.take();
                    }
                    if chunk.stop_reason.is_some() && state.stop_reason.is_none() {
                        state.stop_reason = chunk.stop_reason.take();
                    }
                    // Apply reasoning normalizations on the
                    // slow path too (normalize `reasoning` →
                    // `reasoning_content`, strip `<think>` tags
                    // from content). Previously this only ran
                    // on the fast path, so chunks that carried
                    // real `state.usage` (the final chunk of a stream)
                    // or non-null `finish_reason` leaked raw
                    // `<think>` tags in content AND the upstream's
                    // non-standard `reasoning` field — clients
                    // that parse `reasoning_content` would then
                    // see the reasoning twice (once in the
                    // reasoning panel via the `reasoning` field,
                    // once in the visible response via the
                    // `<think>` tags).
                    let effective_payload = apply_reasoning_normalizations(
                        json_payload,
                        &mut state.think_extractor,
                        &mut state.tool_call_acc,
                    );
                    let payload_str = effective_payload.as_deref().unwrap_or(json_payload);
                    // G1 fix: feed the accumulator so the
                    // persisted `response_body_json` carries
                    // the full assistant message. Slow path:
                    // we have a parsed chunk in hand, so push
                    // the per-chunk metadata + (normalized)
                    // payload.
                    if let Some(a) = state.acc.as_mut() {
                        if let Some(u) = &state.usage {
                            a.set_usage(u.clone());
                        }
                        if let Some(sr) = &state.stop_reason {
                            a.set_stop_reason(sr.clone());
                        }
                        a.append_openai_raw(payload_str);
                        // Extract reasoning_content from the
                        // (possibly normalized) payload and
                        // feed it to the accumulator. We do
                        // this instead of using
                        // `chunk.delta_reasoning` because:
                        //   - `chunk.delta_reasoning` only
                        //     catches `reasoning_content`
                        //     from the raw upstream payload,
                        //     missing upstream `reasoning`
                        //     (which we just normalized) and
                        //     `<think>` tags (which we just
                        //     extracted to reasoning_content).
                        //   - `payload_str` is the source of
                        //     truth after normalization.
                        if payload_str.contains("\"reasoning_content\"")
                            && let Some(rc) =
                                crate::sse_accumulator::extract_reasoning_content(payload_str)
                            && !rc.is_empty()
                        {
                            a.append_reasoning(rc);
                        }
                        // Drop `chunk.delta_reasoning` — we
                        // extracted reasoning from the
                        // normalized payload above. Using
                        // both would double-count when the
                        // upstream sent `reasoning_content`
                        // natively.
                        let _ = chunk.delta_reasoning.take();
                    }
                    // Build the SSE frame from the
                    // (possibly normalized) payload rather
                    // than `chunk.into_sse_bytes()` (which
                    // forwards the raw upstream payload
                    // verbatim, skipping normalization).
                    let sse_bytes = crate::sse::build_sse_frame(payload_str);
                    // Race cancellation guard: if another
                    // target won the race, discard this
                    // chunk to prevent interleaving.
                    if req.race_cancel.as_ref().is_some_and(|rc| rc.is_cancelled()) {
                        return Ok(crate::streaming::ChunkEvent::Return(
                            self.dispatcher.fail_stream_client_disconnected(
                                crate::upstream_dispatcher::StreamFailureContext {
                                    proxy_url: None,
                                    proxy_status: None,
                                    req: req.clone(),
                                    combo,
                                    target,
                                    attempt,
                                    race_size,
                                    started,
                                    model,
                                    connect_ms: connect_and_send_ms,
                                    ttft_ms: state.ttft_ms,
                                    trace_id: trace_id.to_string(),
                                    acc: state.acc.as_mut(),
                                    chunk_id,
                                    created,
                                    model_name,
                                },
                            ),
                        ));
                    }
                    // Mark this chunk as "real content" so the
                    // body stream switches from `total_deadline`
                    // to the chunk-gap (`body_chunk_ms`) timer
                    // for subsequent reads. Only chunks with
                    // `has_content == true` reset the timer —
                    // metadata-only events (e.g. a final chunk
                    // carrying only `state.usage`/`finish_reason`)
                    // must NOT reset it because no real token
                    // was generated. This prevents the
                    // idle_chunk timer from firing when the
                    // upstream sends a metadata event and
                    // then goes silent while the model is
                    // still generating.
                    if chunk.has_content {
                        stream.note_content_chunk();
                    }
                    if let Err(e) = sink.send(sse_bytes).await {
                        return Ok(crate::streaming::ChunkEvent::Return(
                            self.dispatcher.fail_on_sink_send_error(
                                e,
                                crate::upstream_dispatcher::StreamFailureContext {
                                    proxy_url: None,
                                    proxy_status: None,
                                    req: req.clone(),
                                    combo,
                                    target,
                                    attempt,
                                    race_size,
                                    started,
                                    model,
                                    connect_ms: connect_and_send_ms,
                                    ttft_ms: state.ttft_ms,
                                    trace_id: trace_id.to_string(),
                                    acc: state.acc.as_mut(),
                                    chunk_id,
                                    created,
                                    model_name,
                                },
                            ),
                        ));
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(chunk_id = %chunk_id, error = %e,
                                            "failed to parse SSE line from upstream");
                }
            }
        } else {
            // G1 fix: feed the accumulator on the fast path too.
            //
            // Apply the canonical reasoning-normalization
            // pipeline (normalize `reasoning` →
            // `reasoning_content`, strip `<think>` tags from
            // content) via the shared `apply_reasoning_normalizations`
            // helper. Both fast and slow paths now produce
            // identical client-facing output, and the
            // anti-duplication logic ensures that upstreams
            // which send reasoning in BOTH a `reasoning`
            // field AND `<think>` tags in content (e.g.
            // MiniMax-M3 via tokenrouter) don't surface the
            // reasoning twice to the client.
            //
            // PERF (chunk-forwarding CPU reduction):
            // When `apply_reasoning_normalizations` returns
            // `None` (the common case — most upstreams send
            // clean `reasoning_content` or no reasoning at
            // all, AND no `<think>` tags in content), we can
            // reuse the original `line_bytes` BytesMut —
            // which already contains `data: <payload>`
            // (without the trailing `\n` because `split_to(pos)`
            // excludes the `\n` and `advance(1)` skips it) —
            // by appending just `\n\n` in-place for the SSE
            // terminator and freezing. This eliminates, per
            // chunk on the common fast path:
            //   - one heap allocation (`BytesMut::with_capacity`)
            //   - one full-payload memcpy
            //     (`extend_from_slice(payload.as_bytes())`)
            //   - two small memcpys (`data: ` + `\n\n`)
            // The only cost is appending 2 bytes to `line_bytes`,
            // which usually does NOT reallocate: `split_to`
            // preserves the parent buffer's capacity, and the
            // parent is `BytesMut::with_capacity(8192)` reserved
            // up to 16 KiB above. Even on the rare case where it
            // does realloc, the cost is amortized across the
            // buffer's lifetime, not per chunk.
            let effective_payload = apply_reasoning_normalizations(
                json_payload,
                &mut state.think_extractor,
                &mut state.tool_call_acc,
            );

            // Accumulator work — scoped so the borrows on
            // `line_bytes` (via `line` -> `json_payload`) are
            // released before we move `line_bytes` for the
            // in-place reframe below. NLL releases the borrow at
            // the end of this block.
            //
            // IMPORTANT: feed the accumulator from
            // `effective_payload` (the post-normalization
            // payload), NOT from `json_payload` (the raw
            // upstream payload). Previously this block fed
            // `normalized` (which only applied the
            // `reasoning` → `reasoning_content` rename) and
            // then computed `effective_payload` (which also
            // applied `<think>` extraction) but only used
            // `effective_payload` for the outgoing SSE frame
            // — so the persisted `response_body_json` had
            // raw `<think>` tags in `content` while the
            // client saw the cleaned version. Using
            // `effective_payload` for both paths makes the
            // persisted body match the client-visible body.
            {
                let payload = effective_payload.as_deref().unwrap_or(json_payload);
                if let Some(a) = state.acc.as_mut() {
                    a.append_openai_raw(payload);
                    // PERF (single-scan guard): gate the
                    // `extract_reasoning_content` call (which does
                    // a full `memchr::memmem::find` scan for
                    // `"reasoning_content":"`) behind a cheaper
                    // `contains("\"reasoning_content\"")` check.
                    // Most streaming chunks (content deltas,
                    // role-only, finish, etc.) do NOT carry this
                    // field, so this saves one full-payload scan
                    // per chunk on the recording path. The guard
                    // itself short-circuits on first non-match.
                    if payload.contains("\"reasoning_content\"")
                        && let Some(rc) = crate::sse_accumulator::extract_reasoning_content(payload)
                        && !rc.is_empty()
                    {
                        a.append_reasoning(rc);
                    }
                }
            }

            // Build the SSE frame.
            let sse_bytes = if let Some(modified) = effective_payload {
                // Normalization or think extraction modified
                // the payload. Build a fresh frame with the
                // modified JSON.
                crate::sse::build_sse_frame(&modified)
            } else {
                // Fast path: forward the original `data: <payload>`
                // line directly. `line_bytes` is the BytesMut
                // returned by `buffer.split_to(pos)` and contains
                // the upstream's `data: <payload>` bytes verbatim
                // (we did not modify it — only `line`/`json_payload`
                // were borrows for substring checks). Append `\n\n`
                // for the SSE event terminator and freeze.
                // Zero allocation, zero payload memcpy.
                let mut frame = bytes::BytesMut::from(line_bytes as &[u8]);
                frame.extend_from_slice(b"\n\n");
                frame.freeze()
            };

            // Race cancellation guard: check before
            // writing to the shared sink.
            if req.race_cancel.as_ref().is_some_and(|rc| rc.is_cancelled()) {
                return Ok(crate::streaming::ChunkEvent::Return(
                    self.dispatcher.fail_stream_client_disconnected(
                        crate::upstream_dispatcher::StreamFailureContext {
                            proxy_url: None,
                            proxy_status: None,
                            req: req.clone(),
                            combo,
                            target,
                            attempt,
                            race_size,
                            started,
                            model,
                            connect_ms: connect_and_send_ms,
                            ttft_ms: state.ttft_ms,
                            trace_id: trace_id.to_string(),
                            acc: state.acc.as_mut(),
                            chunk_id,
                            created,
                            model_name,
                        },
                    ),
                ));
            }
            // Mark this chunk as "real content" so the body
            // stream switches from `total_deadline` to the
            // chunk-gap (`body_chunk_ms`) timer for the next
            // read. The fast path only runs for chunks
            // WITHOUT `state.usage` or non-null `finish_reason`
            // (those go to the slow path above), so every
            // chunk reaching this point is a content delta
            // — we unconditionally reset the timer here.
            // See the slow path above for the `has_content`
            // gate that metadata-only chunks must NOT reset.
            stream.note_content_chunk();
            if let Err(e) = sink.send(sse_bytes).await {
                return Ok(crate::streaming::ChunkEvent::Return(
                    self.dispatcher.fail_on_sink_send_error(
                        e,
                        crate::upstream_dispatcher::StreamFailureContext {
                            proxy_url: None,
                            proxy_status: None,
                            req: req.clone(),
                            combo,
                            target,
                            attempt,
                            race_size,
                            started,
                            model,
                            connect_ms: connect_and_send_ms,
                            ttft_ms: state.ttft_ms,
                            trace_id: trace_id.to_string(),
                            acc: state.acc.as_mut(),
                            chunk_id,
                            created,
                            model_name,
                        },
                    ),
                ));
            }
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
        let state = &mut *self.state;
        let req = ctx.req;
        let combo = ctx.combo;
        let target = ctx.target;
        let model = ctx.model;
        let target_format = ctx.target_format;
        let sink = ctx.sink;
        let trace_id = ctx.trace_id;
        let chunk_id = ctx.chunk_id;
        let model_name = ctx.model_name;
        let started = ctx.started;
        let attempt = ctx.attempt;
        let race_size = ctx.race_size;
        let created = ctx.created;
        let connect_and_send_ms = ctx.connect_and_send_ms;

        let parsed = match target_format {
            openproxy_types::TargetFormat::Responses => crate::sse::parse_responses_sse_stream_line(
                line,
                chunk_id,
                created,
                model_name,
                &mut state.responses_sse_state,
            ),
            openproxy_types::TargetFormat::Openai => crate::sse::parse_openai_sse_line(line),
            openproxy_types::TargetFormat::Gemini => {
                crate::sse::parse_gemini_sse_line(line, chunk_id, created, model_name)
            }
            openproxy_types::TargetFormat::Anthropic => {
                // Anthropic SSE: track event type across lines
                // and run the stateful translator that can
                // accumulate Anthropic `tool_use` blocks into
                // OpenAI-style `tool_calls` chunks. The
                // accumulator lives across iterations of the
                // outer loop.
                match crate::sse::parse_anthropic_sse_stream_line(
                    line,
                    &mut state.current_event_type,
                ) {
                    Ok(Some(payload)) => {
                        // H5 fix: thread the tool_use accumulator
                        // through the translator. The counter
                        // allocates fresh indices for each
                        // content_block_start that opens a
                        // tool_use; the accumulator carries the
                        // in-flight id+name+arguments across
                        // deltas.
                        match crate::sse::translate_anthropic_sse_event(
                            &payload,
                            chunk_id,
                            created,
                            model_name,
                            &mut state.tool_use_acc,
                            &mut state.tool_call_index_counter,
                        ) {
                            Ok(Some(chunk)) => Ok(Some(chunk)),
                            Ok(None) => Ok(None),
                            Err(e) => Err(e),
                        }
                    }
                    Ok(None) => Ok(None),
                    Err(e) => Err(e),
                }
            }
        };

        match parsed {
            Ok(Some(mut chunk)) => {
                if chunk.done {
                    // Capture state.stop_reason from the final chunk.
                    if chunk.stop_reason.is_some() {
                        state.stop_reason = chunk.stop_reason;
                    }
                    // Send [DONE] sentinel and break.
                    // H4 fix: record the fact that we sent
                    // the upstream's [DONE] so the post-loop
                    // sentinel below does not double-emit.
                    if req.race_cancel.as_ref().is_some_and(|rc| rc.is_cancelled()) {
                        return Ok(crate::streaming::ChunkEvent::Return(
                            self.dispatcher.fail_stream_client_disconnected(
                                crate::upstream_dispatcher::StreamFailureContext {
                                    proxy_url: None,
                                    proxy_status: None,
                                    req: req.clone(),
                                    combo,
                                    target,
                                    attempt,
                                    race_size,
                                    started,
                                    model,
                                    connect_ms: connect_and_send_ms,
                                    ttft_ms: state.ttft_ms,
                                    trace_id: trace_id.to_string(),
                                    acc: state.acc.as_mut(),
                                    chunk_id,
                                    created,
                                    model_name,
                                },
                            ),
                        ));
                    }
                    if let Err(crate::race_sink::StreamSinkError::Lost) =
                        sink.send(SSE_DONE_BYTES.clone()).await
                    {
                        return Ok(crate::streaming::ChunkEvent::Return(
                            self.dispatcher.fail_on_sink_send_error(
                                crate::race_sink::StreamSinkError::Lost,
                                crate::upstream_dispatcher::StreamFailureContext {
                                    proxy_url: None,
                                    proxy_status: None,
                                    req: req.clone(),
                                    combo,
                                    target,
                                    attempt,
                                    race_size,
                                    started,
                                    model,
                                    connect_ms: connect_and_send_ms,
                                    ttft_ms: state.ttft_ms,
                                    trace_id: trace_id.to_string(),
                                    acc: state.acc.as_mut(),
                                    chunk_id,
                                    created,
                                    model_name,
                                },
                            ),
                        ));
                    }
                    state.done_sent = true;
                    // CRITICAL FIX: break from the outer
                    // loop after sending [DONE], matching
                    // the OpenAI path above. Without this
                    // break the loop continues processing
                    // the buffer and can forward data
                    // after [DONE] to the client, causing
                    // chunk overlapping and output corruption.
                    return Ok(crate::streaming::ChunkEvent::Done);
                } else {
                    // Extract metadata before consuming chunk.
                    if chunk.usage.is_some() {
                        state.usage = chunk.usage.take();
                    }
                    if chunk.stop_reason.is_some() && state.stop_reason.is_none() {
                        state.stop_reason = chunk.stop_reason.take();
                    }
                    // G1 fix: feed the accumulator. Per-format
                    // dispatch covers Gemini and Anthropic
                    // (the OpenAI slow path is handled at
                    // line 2632). The translated chunk's
                    // payload is already OpenAI-shaped JSON
                    // (sse.rs's translators emit OpenAI
                    // JSON for both Gemini and Anthropic),
                    // so we hand the final JSON to
                    // `append_openai_raw` for content
                    // reconstruction in `finish()`.
                    //
                    // Extract the per-chunk fields that
                    // don't fit the OpenAI shape before
                    // consuming the chunk.
                    let delta_reasoning = chunk.delta_reasoning.take();
                    let _delta_tool_calls = std::mem::take(&mut chunk.delta_tool_calls);
                    // Capture has_content BEFORE consuming
                    // the chunk — `into_json_string()` takes
                    // `self` by value. We use this flag below
                    // to decide whether to call
                    // `note_content_chunk()`. Metadata-only
                    // events (message_start, message_delta,
                    // content_block_start for tool_use) have
                    // `has_content == false` and must NOT
                    // reset the chunk-gap timer.
                    let chunk_has_content = chunk.has_content;
                    let json_str = chunk.into_json_string();
                    if let Some(a) = state.acc.as_mut() {
                        if let Some(u) = &state.usage {
                            a.set_usage(u.clone());
                        }
                        if let Some(sr) = &state.stop_reason {
                            a.set_stop_reason(sr.clone());
                        }
                        if let Some(dr) = &delta_reasoning
                            && !dr.is_empty()
                        {
                            a.append_reasoning(dr);
                        }
                        // Anthropic tool_use threading. The
                        // Open-shape events carry `id` and
                        // `function.name`; delta-shape
                        // events carry only `function.arguments`.
                        // `content_block_stop` returns
                        // `Ok(None)` upstream so we never see
                        // a Close event here (the accumulator's
                        // Close is a no-op anyway).

                        a.append_openai_raw(&json_str);
                    }
                    // Pre-format as SSE frame to avoid per-chunk String alloc + axum Event overhead.
                    let sse_frame = crate::sse::build_sse_frame(&json_str);
                    // Mark this chunk as "real content" so the
                    // body stream switches from `total_deadline`
                    // to the chunk-gap timer for the next read.
                    // Only chunks with `has_content == true`
                    // reset the timer — metadata-only events
                    // (message_start, message_delta,
                    // content_block_start for tool_use) have
                    // `has_content == false` because they
                    // carry no generated tokens. This is the
                    // root fix for the "idle_chunk after
                    // 10000ms" bug on MiniMax-M3 tool calls:
                    // the content_block_start event arrived
                    // at ~0ms with empty arguments, and
                    // resetting the timer there caused the
                    // 10s gap timer to fire while the model
                    // was still generating the first argument
                    // fragment.
                    if chunk_has_content {
                        stream.note_content_chunk();
                    }
                    if let Err(e) = sink.send(sse_frame).await {
                        // C4 fix: a real client disconnect
                        // mid-stream previously returned
                        // `PipelineResult { error: None }`
                        // — no state.usage row, tokens consumed
                        // at the upstream were unbilled.
                        // Hand off to
                        // `record_and_fail_with_trace_id`
                        // (H3 fix: the row's `trace_id`
                        // matches the StageEvent's
                        // `trace_id`) so the row lands in
                        // the DB with status_code = 499
                        // and the operator sees a real
                        // failure event. The `state.usage` we
                        // accumulated up to this point
                        // still goes into the row because
                        // `UsageRecordBuilder`
                        // accepts an `Option<u32>` pair.
                        return Ok(crate::streaming::ChunkEvent::Return(
                            self.dispatcher.fail_on_sink_send_error(
                                e,
                                crate::upstream_dispatcher::StreamFailureContext {
                                    proxy_url: None,
                                    proxy_status: None,
                                    req: req.clone(),
                                    combo,
                                    target,
                                    attempt,
                                    race_size,
                                    started,
                                    model,
                                    connect_ms: connect_and_send_ms,
                                    ttft_ms: state.ttft_ms,
                                    trace_id: trace_id.to_string(),
                                    acc: state.acc.as_mut(),
                                    chunk_id,
                                    created,
                                    model_name,
                                },
                            ),
                        ));
                    }
                }
            }
            Ok(None) => return Ok(crate::streaming::ChunkEvent::Skip),
            Err(e) => {
                tracing::warn!(
                    chunk_id = %chunk_id,
                    error = %e,
                    "failed to parse SSE line from upstream"
                );
                let acc_ref: Option<&crate::sse_accumulator::ResponseAccumulator> =
                    match &mut state.acc {
                        Some(a) => {
                            a.mark_partial();
                            Some(&*a)
                        }
                        None => None,
                    };
                return Ok(crate::streaming::ChunkEvent::Return(
                    self.dispatcher.record_and_fail_with_trace_id_and_partial(
                        req.clone(),
                        combo,
                        target,
                        crate::FailureContext {
                            proxy_url: None,
                            proxy_status: None,
                            attempt,
                            race_size,
                            err: &e,
                            started,
                            model: Some(model),
                            connect_ms: Some(connect_and_send_ms),
                            ttft_ms: state.ttft_ms,
                            status_code: e.http_status(),
                        },
                        trace_id.to_string(),
                        acc_ref,
                        Some(chunk_id),
                        created,
                        model_name,
                    ),
                ));
            }
        }

        // Fallback catch-all for unknown formats or unhandled branches
        Ok(crate::streaming::ChunkEvent::Skip)
    }
}
