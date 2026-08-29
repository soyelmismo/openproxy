use crate::translation::anthropic::map_finish_reason;
use crate::translation::types::{AnthropicResponse, AnthropicUsage};
use bytes::Bytes;
use futures_util::stream::Stream;
use openproxy_types::error::{CoreError, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::pin::Pin;
use std::task::{Context, Poll};

// Anthropic SSE event types
// =====================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicSseEvent {
    MessageStart {
        message: AnthropicResponse,
    },
    ContentBlockStart {
        index: u32,
        content_block: serde_json::Value,
    },
    ContentBlockDelta {
        index: u32,
        /// {type: "text_delta", text: "..."}
        delta: serde_json::Value,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        /// Contains stop_reason.
        delta: serde_json::Value,
        usage: Option<AnthropicUsage>,
    },
    MessageStop,
    Ping,
}

fn build_role_chunk(chunk_id: &str, created: u64, model: &str) -> Vec<String> {
    let chunk = serde_json::json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": { "role": "assistant" },
            "finish_reason": serde_json::Value::Null
        }]
    });
    vec![format_sse_data(&chunk)]
}

fn build_content_chunk(
    chunk_id: &str,
    created: u64,
    model: &str,
    delta: &serde_json::Value,
) -> Vec<String> {
    let text = delta
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    let chunk = serde_json::json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": { "content": text },
            "finish_reason": serde_json::Value::Null
        }]
    });
    vec![format_sse_data(&chunk)]
}

fn build_usage_json(u: &AnthropicUsage) -> serde_json::Value {
    let cache_read = u.cache_read_input_tokens.unwrap_or(0);
    let cache_creation = u.cache_creation_input_tokens.unwrap_or(0);
    let prompt = u
        .input_tokens
        .saturating_add(cache_read)
        .saturating_add(cache_creation);
    let completion = u.output_tokens;
    let total = prompt.saturating_add(completion);

    let mut usage_json = serde_json::json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": total,
    });
    if let Some(cached) = u.cache_read_input_tokens {
        usage_json["prompt_tokens_details"] = serde_json::json!({
            "cached_tokens": cached
        });
    }
    usage_json
}

fn build_message_delta_chunk(
    chunk_id: &str,
    created: u64,
    model: &str,
    delta: &serde_json::Value,
    usage: Option<&AnthropicUsage>,
) -> Vec<String> {
    let stop_reason = delta
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .map(map_finish_reason);

    let choice = match stop_reason {
        Some(reason) => json!({
            "index": 0,
            "delta": {},
            "finish_reason": reason,
        }),
        None => json!({
            "index": 0,
            "delta": {},
            "finish_reason": serde_json::Value::Null,
        }),
    };

    let mut chunk = serde_json::json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [choice],
    });

    if let Some(u) = usage {
        chunk["usage"] = build_usage_json(u);
    }

    vec![format_sse_data(&chunk)]
}

fn build_message_stop_chunks(chunk_id: &str, created: u64, model: &str) -> Vec<String> {
    let chunk = serde_json::json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "stop",
        }],
    });
    vec![format_sse_data(&chunk), "data: [DONE]\n\n".to_string()]
}

pub use anthropic_sse_event_to_openai_chunks as anthropic_sse_to_openai_chunks;

pub fn anthropic_sse_event_to_openai_chunks(
    event: &AnthropicSseEvent,
    chunk_id: &str,
    created: u64,
    model: &str,
) -> Vec<String> {
    match event {
        AnthropicSseEvent::Ping
        | AnthropicSseEvent::ContentBlockStart { .. }
        | AnthropicSseEvent::ContentBlockStop { .. } => Vec::new(),
        AnthropicSseEvent::MessageStart { .. } => build_role_chunk(chunk_id, created, model),
        AnthropicSseEvent::ContentBlockDelta { delta, .. } => {
            build_content_chunk(chunk_id, created, model, delta)
        }
        AnthropicSseEvent::MessageDelta { delta, usage } => {
            build_message_delta_chunk(chunk_id, created, model, delta, usage.as_ref())
        }
        AnthropicSseEvent::MessageStop => build_message_stop_chunks(chunk_id, created, model),
    }
}

/// Parse a raw SSE `data:` line (with or without the `data: ` prefix) into an
/// [`AnthropicSseEvent`]. Returns:
///
/// - `Ok(Some(event))` for valid event payloads.
/// - `Ok(None)` for lines that should be ignored (ping, comments, empty payload,
///   non-`data:` lines, `[DONE]` sentinel).
/// - `Err(CoreError::Parse(_))` for malformed JSON or event payload that should
///   be a valid event.
fn is_ping_payload(payload: &[u8]) -> Result<bool> {
    #[derive(serde::Deserialize)]
    struct TypeProbe<'a> {
        #[serde(borrow)]
        r#type: Option<std::borrow::Cow<'a, str>>,
    }
    let probe: TypeProbe = serde_json::from_slice(payload)
        .map_err(|e| CoreError::Parse(format!("invalid SSE JSON: {e}")))?;
    Ok(probe.r#type.as_deref() == Some("ping"))
}

/// Parse a raw SSE `data:` line (with or without the `data: ` prefix) into an
/// [`AnthropicSseEvent`]. Returns:
///
/// - `Ok(Some(event))` for valid event payloads.
/// - `Ok(None)` for lines that should be ignored (ping, comments, empty payload,
///   non-`data:` lines, `[DONE]` sentinel).
/// - `Err(CoreError::Parse(_))` for malformed JSON or event payload that should
///   be a valid event.
pub fn parse_anthropic_sse_line(line: &str) -> Result<Option<AnthropicSseEvent>> {
    let Some(payload) = crate::sse::parse_sse_data_line(line) else {
        return Ok(None);
    };

    if payload == "[DONE]" || is_ping_payload(payload.as_bytes())? {
        return Ok(None);
    }

    let event: AnthropicSseEvent = serde_json::from_slice(payload.as_bytes())
        .map_err(|e| CoreError::Parse(format!("invalid Anthropic SSE event: {e}")))?;

    Ok(Some(event))
}

fn format_sse_data(payload: &serde_json::Value) -> String {
    format!("data: {payload}\n\n")
}

fn append_sse_event(out: &mut bytes::BytesMut, event_name: &str, payload: &serde_json::Value) {
    out.extend_from_slice(format!("event: {event_name}\ndata: ").as_bytes());
    out.extend_from_slice(
        serde_json::to_string(payload)
            .unwrap_or_else(|_| r#"{"type":"error","error":{"type":"internal_error","message":"Internal server error"}}"#.to_string())
            .as_bytes(),
    );
    out.extend_from_slice(b"\n\n");
}

pub struct OpenAIToAnthropicSseStream<S> {
    pub inner: S,
    pub has_started: bool,
    pub has_finished: bool,
    pub message_id: String,
    pub model: String,
    pub block_index: u32,
    pub in_text_block: bool,
    pub in_tool_block: bool,
}

impl<S> OpenAIToAnthropicSseStream<S> {
    pub fn new(inner: S, message_id: String, model: String) -> Self {
        Self {
            inner,
            has_started: false,
            has_finished: false,
            message_id,
            model,
            block_index: 0,
            in_text_block: false,
            in_tool_block: false,
        }
    }

    fn emit_message_start(&mut self, out: &mut bytes::BytesMut) {
        if self.has_started {
            return;
        }
        self.has_started = true;
        let start_event = serde_json::json!({
            "type": "message_start",
            "message": {
                "id": self.message_id,
                "type": "message",
                "role": "assistant",
                "model": self.model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        });
        append_sse_event(out, "message_start", &start_event);

        self.in_text_block = true;
        self.block_index = 0;
        let block_start = serde_json::json!({
            "type": "content_block_start",
            "index": self.block_index,
            "content_block": {"type": "text", "text": ""}
        });
        append_sse_event(out, "content_block_start", &block_start);
    }

    fn handle_content_delta(&mut self, content: &str, out: &mut bytes::BytesMut) {
        if content.is_empty() {
            return;
        }
        if self.in_tool_block {
            let stop = serde_json::json!({"type": "content_block_stop", "index": self.block_index});
            append_sse_event(out, "content_block_stop", &stop);
            self.in_tool_block = false;
            self.block_index += 1;
        }
        if !self.in_text_block {
            self.in_text_block = true;
            let start = serde_json::json!({
                "type": "content_block_start",
                "index": self.block_index,
                "content_block": {"type": "text", "text": ""}
            });
            append_sse_event(out, "content_block_start", &start);
        }
        let block_delta = serde_json::json!({
            "type": "content_block_delta",
            "index": self.block_index,
            "delta": {"type": "text_delta", "text": content}
        });
        append_sse_event(out, "content_block_delta", &block_delta);
    }

    fn transition_tool_block(&mut self, id: &str, name: &str, out: &mut bytes::BytesMut) {
        if self.in_text_block || self.in_tool_block {
            let stop = serde_json::json!({"type": "content_block_stop", "index": self.block_index});
            append_sse_event(out, "content_block_stop", &stop);
            self.in_text_block = false;
            self.block_index += 1;
        }
        self.in_tool_block = true;
        let start = serde_json::json!({
            "type": "content_block_start",
            "index": self.block_index,
            "content_block": {
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": {}
            }
        });
        append_sse_event(out, "content_block_start", &start);
    }

    fn append_tool_call_arguments_delta(&self, args: &str, out: &mut bytes::BytesMut) {
        let block_delta = serde_json::json!({
            "type": "content_block_delta",
            "index": self.block_index,
            "delta": {"type": "input_json_delta", "partial_json": args}
        });
        append_sse_event(out, "content_block_delta", &block_delta);
    }

    fn handle_tool_call_item(&mut self, tc: &OpenAIToolCallProbe<'_>, out: &mut bytes::BytesMut) {
        if let Some(id) = &tc.id {
            let name = tc
                .function
                .as_ref()
                .and_then(|f| f.name.as_deref())
                .unwrap_or_default();
            self.transition_tool_block(id, name, out);
        }

        if let Some(func) = &tc.function
            && let Some(args) = &func.arguments
            && !args.is_empty()
        {
            self.append_tool_call_arguments_delta(args, out);
        }
    }

    fn handle_finish_reason(&mut self, finish_reason: &str, out: &mut bytes::BytesMut) {
        if self.has_finished {
            return;
        }
        self.has_finished = true;

        if self.in_text_block || self.in_tool_block {
            let stop = serde_json::json!({
                "type": "content_block_stop",
                "index": self.block_index
            });
            append_sse_event(out, "content_block_stop", &stop);
        }

        let anthropic_stop = match finish_reason {
            "length" => "max_tokens",
            "tool_calls" | "function_call" => "tool_use",
            "content_filter" => "stop_sequence",
            _ => "end_turn",
        };

        let msg_delta = serde_json::json!({
            "type": "message_delta",
            "delta": {"stop_reason": anthropic_stop},
            "usage": {"output_tokens": 0}
        });
        append_sse_event(out, "message_delta", &msg_delta);
        out.extend_from_slice(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    }

    fn process_openai_probe(&mut self, v: OpenAISseProbe<'_>, out: &mut bytes::BytesMut) {
        self.emit_message_start(out);
        let Some(first) = v.choices.as_ref().and_then(|c| c.first()) else {
            return;
        };

        if let Some(delta) = &first.delta {
            if let Some(content) = delta.content.as_deref() {
                self.handle_content_delta(content, out);
            }
            if let Some(tool_calls) = &delta.tool_calls {
                for tc in tool_calls {
                    self.handle_tool_call_item(tc, out);
                }
            }
        }

        if let Some(finish_reason) = first.finish_reason.as_deref() {
            self.handle_finish_reason(finish_reason, out);
        }
    }

    fn process_raw_chunk(&mut self, chunk: &Bytes) -> Option<Bytes> {
        let s = String::from_utf8_lossy(chunk);
        let s = s.as_ref();
        if s.starts_with("data: ") && !s.contains("[DONE]") {
            let json_str = s.trim_start_matches("data: ").trim();
            if let Ok(v) = serde_json::from_slice::<OpenAISseProbe<'_>>(json_str.as_bytes()) {
                let mut out = bytes::BytesMut::new();
                self.process_openai_probe(v, &mut out);
                return Some(out.freeze());
            }
        }
        if s.starts_with("event: error") || s.starts_with(": keep-alive") {
            return Some(chunk.clone());
        }
        None
    }
}

impl<S: Stream<Item = Bytes> + Unpin> Stream for OpenAIToAnthropicSseStream<S> {
    type Item = std::result::Result<Bytes, std::convert::Infallible>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(chunk)) => {
                    if let Some(out) = this.process_raw_chunk(&chunk) {
                        return Poll::Ready(Some(Ok(out)));
                    }
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAISseProbe<'a> {
    #[serde(borrow)]
    pub choices: Option<Vec<OpenAIChoiceProbe<'a>>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAIChoiceProbe<'a> {
    #[serde(borrow)]
    pub delta: Option<OpenAIDeltaProbe<'a>>,
    #[serde(borrow)]
    pub finish_reason: Option<std::borrow::Cow<'a, str>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAIDeltaProbe<'a> {
    #[serde(borrow)]
    pub content: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    pub tool_calls: Option<Vec<OpenAIToolCallProbe<'a>>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAIToolCallProbe<'a> {
    #[serde(borrow)]
    pub id: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    pub function: Option<OpenAIFunctionCallProbe<'a>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAIFunctionCallProbe<'a> {
    #[serde(borrow)]
    pub name: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    pub arguments: Option<std::borrow::Cow<'a, str>>,
}
