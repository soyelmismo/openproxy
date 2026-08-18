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

/// Convert a single Anthropic SSE event to zero or more OpenAI SSE chunks.
///
/// Each returned string is a full SSE frame: `data: {json}\n\n`. Returns an
/// empty `Vec` if the event should be skipped (e.g. `ping`).
///
/// `chunk_id`, `created`, and `model` are taken from the outer response context
/// since Anthropic events don't repeat them on every frame.
pub fn anthropic_sse_to_openai_chunks(
    event: &AnthropicSseEvent,
    chunk_id: &str,
    created: u64,
    model: &str,
) -> Vec<String> {
    match event {
        AnthropicSseEvent::Ping => Vec::new(),

        AnthropicSseEvent::MessageStart { .. } => {
            // Emit a role-only chunk so clients can start streaming immediately.
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

        AnthropicSseEvent::ContentBlockStart { .. }
        | AnthropicSseEvent::ContentBlockStop { .. } => {
            // No-op boundaries: text is delivered through deltas only.
            Vec::new()
        }

        AnthropicSseEvent::ContentBlockDelta { delta, .. } => {
            // Extract the text fragment if the delta is a text_delta.
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

        AnthropicSseEvent::MessageDelta { delta, usage } => {
            // The delta carries stop_reason. Forward it as a finish_reason chunk.
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
                chunk["usage"] = usage_json;
            }

            vec![format_sse_data(&chunk)]
        }

        AnthropicSseEvent::MessageStop => {
            // Final terminator. We send a chunk with finish_reason=stop and the
            // [DONE] sentinel so both common client patterns work.
            let chunk = serde_json::json!({
                "id": chunk_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "stop"
                }]
            });
            vec![format_sse_data(&chunk), "data: [DONE]\n\n".to_string()]
        }
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
pub fn parse_anthropic_sse_line(line: &str) -> Result<Option<AnthropicSseEvent>> {
    let Some(payload) = crate::sse::parse_sse_data_line(line) else {
        return Ok(None);
    };

    // The OpenAI-style [DONE] sentinel is sometimes emitted by intermediate proxies.
    if payload == "[DONE]" {
        return Ok(None);
    }

    // Probe the JSON for the discriminator. A "ping" event from Anthropic
    // must be ignored, not surfaced as a parse error.
    let probe: serde_json::Value = serde_json::from_str(payload)
        .map_err(|e| CoreError::Parse(format!("invalid SSE JSON: {e}")))?;

    if let Some(t) = probe.get("type").and_then(|v| v.as_str())
        && t == "ping"
    {
        return Ok(None);
    }

    let event: AnthropicSseEvent =
        <AnthropicSseEvent as serde::Deserialize>::deserialize(&probe)
            .map_err(|e| CoreError::Parse(format!("invalid Anthropic SSE event: {e}")))?;

    Ok(Some(event))
}

fn format_sse_data(payload: &serde_json::Value) -> String {
    format!("data: {payload}\n\n")
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
}

impl<S: Stream<Item = Bytes> + Unpin> Stream for OpenAIToAnthropicSseStream<S> {
    type Item = std::result::Result<Bytes, std::convert::Infallible>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(chunk)) => {
                    let s = std::str::from_utf8(&chunk).unwrap_or("");
                    if s.starts_with("data: ") && !s.contains("[DONE]") {
                        let json_str = s.trim_start_matches("data: ").trim();
                        if let Ok(v) = serde_json::from_str::<OpenAISseProbe>(json_str) {
                            let mut out = bytes::BytesMut::new();

                            if !this.has_started {
                                this.has_started = true;
                                let start_event = serde_json::json!({
                                    "type": "message_start",
                                    "message": {
                                        "id": this.message_id,
                                        "type": "message",
                                        "role": "assistant",
                                        "model": this.model,
                                        "content": [],
                                        "stop_reason": null,
                                        "stop_sequence": null,
                                        "usage": {"input_tokens": 0, "output_tokens": 0}
                                    }
                                });
                                out.extend_from_slice(b"event: message_start\ndata: ");
                                out.extend_from_slice(
                                    serde_json::to_string(&start_event).unwrap_or_else(|_| r#"{"type":"error","error":{"type":"internal_error","message":"Internal server error"}}"#.to_string()).as_bytes(),
                                );
                                out.extend_from_slice(b"\n\n");

                                this.in_text_block = true;
                                this.block_index = 0;
                                let block_start = serde_json::json!({
                                    "type": "content_block_start",
                                    "index": this.block_index,
                                    "content_block": {"type": "text", "text": ""}
                                });
                                out.extend_from_slice(b"event: content_block_start\ndata: ");
                                out.extend_from_slice(
                                    serde_json::to_string(&block_start).unwrap_or_else(|_| r#"{"type":"error","error":{"type":"internal_error","message":"Internal server error"}}"#.to_string()).as_bytes(),
                                );
                                out.extend_from_slice(b"\n\n");
                            }

                            if let Some(choices) = v.choices
                                && let Some(first) = choices.first()
                            {
                                if let Some(delta) = &first.delta {
                                    if let Some(content) = &delta.content
                                        && !content.is_empty()
                                    {
                                        if this.in_tool_block {
                                            let stop = serde_json::json!({"type": "content_block_stop", "index": this.block_index});
                                            out.extend_from_slice(
                                                b"event: content_block_stop\ndata: ",
                                            );
                                            out.extend_from_slice(
                                                serde_json::to_string(&stop).unwrap_or_else(|_| r#"{"type":"error","error":{"type":"internal_error","message":"Internal server error"}}"#.to_string()).as_bytes(),
                                            );
                                            out.extend_from_slice(b"\n\n");
                                            this.in_tool_block = false;
                                            this.block_index += 1;
                                        }
                                        if !this.in_text_block {
                                            this.in_text_block = true;
                                            let start = serde_json::json!({
                                                "type": "content_block_start",
                                                "index": this.block_index,
                                                "content_block": {"type": "text", "text": ""}
                                            });
                                            out.extend_from_slice(
                                                b"event: content_block_start\ndata: ",
                                            );
                                            out.extend_from_slice(
                                                serde_json::to_string(&start).unwrap_or_else(|_| r#"{"type":"error","error":{"type":"internal_error","message":"Internal server error"}}"#.to_string()).as_bytes(),
                                            );
                                            out.extend_from_slice(b"\n\n");
                                        }
                                        let block_delta = serde_json::json!({
                                            "type": "content_block_delta",
                                            "index": this.block_index,
                                            "delta": {"type": "text_delta", "text": content}
                                        });
                                        out.extend_from_slice(
                                            b"event: content_block_delta\ndata: ",
                                        );
                                        out.extend_from_slice(
                                            serde_json::to_string(&block_delta).unwrap_or_else(|_| r#"{"type":"error","error":{"type":"internal_error","message":"Internal server error"}}"#.to_string()).as_bytes(),
                                        );
                                        out.extend_from_slice(b"\n\n");
                                    }

                                    if let Some(tool_calls) = &delta.tool_calls {
                                        for tc in tool_calls {
                                            if let Some(id) = &tc.id {
                                                if this.in_text_block {
                                                    let stop = serde_json::json!({"type": "content_block_stop", "index": this.block_index});
                                                    out.extend_from_slice(
                                                        b"event: content_block_stop\ndata: ",
                                                    );
                                                    out.extend_from_slice(
                                                        serde_json::to_string(&stop).unwrap_or_else(|_| r#"{"type":"error","error":{"type":"internal_error","message":"Internal server error"}}"#.to_string())
                                                            .as_bytes(),
                                                    );
                                                    out.extend_from_slice(b"\n\n");
                                                    this.in_text_block = false;
                                                    this.block_index += 1;
                                                }
                                                if this.in_tool_block {
                                                    let stop = serde_json::json!({"type": "content_block_stop", "index": this.block_index});
                                                    out.extend_from_slice(
                                                        b"event: content_block_stop\ndata: ",
                                                    );
                                                    out.extend_from_slice(
                                                        serde_json::to_string(&stop).unwrap_or_else(|_| r#"{"type":"error","error":{"type":"internal_error","message":"Internal server error"}}"#.to_string())
                                                            .as_bytes(),
                                                    );
                                                    out.extend_from_slice(b"\n\n");
                                                    this.block_index += 1;
                                                }
                                                this.in_tool_block = true;
                                                let name = tc
                                                    .function
                                                    .as_ref()
                                                    .and_then(|f| f.name.as_deref())
                                                    .unwrap_or_default();
                                                let start = serde_json::json!({
                                                    "type": "content_block_start",
                                                    "index": this.block_index,
                                                    "content_block": {
                                                        "type": "tool_use",
                                                        "id": id,
                                                        "name": name,
                                                        "input": {}
                                                    }
                                                });
                                                out.extend_from_slice(
                                                    b"event: content_block_start\ndata: ",
                                                );
                                                out.extend_from_slice(
                                                    serde_json::to_string(&start).unwrap_or_else(|_| r#"{"type":"error","error":{"type":"internal_error","message":"Internal server error"}}"#.to_string())
                                                        .as_bytes(),
                                                );
                                                out.extend_from_slice(b"\n\n");
                                            }

                                            if let Some(func) = &tc.function
                                                && let Some(args) = &func.arguments
                                                && !args.is_empty()
                                            {
                                                let block_delta = serde_json::json!({
                                                    "type": "content_block_delta",
                                                    "index": this.block_index,
                                                    "delta": {"type": "input_json_delta", "partial_json": args}
                                                });
                                                out.extend_from_slice(
                                                    b"event: content_block_delta\ndata: ",
                                                );
                                                out.extend_from_slice(
                                                    serde_json::to_string(&block_delta).unwrap_or_else(|_| r#"{"type":"error","error":{"type":"internal_error","message":"Internal server error"}}"#.to_string())
                                                        .as_bytes(),
                                                );
                                                out.extend_from_slice(b"\n\n");
                                            }
                                        }
                                    }
                                }

                                if let Some(finish_reason) = &first.finish_reason
                                    && !this.has_finished
                                {
                                    this.has_finished = true;

                                    if this.in_text_block || this.in_tool_block {
                                        let stop = serde_json::json!({
                                            "type": "content_block_stop",
                                            "index": this.block_index
                                        });
                                        out.extend_from_slice(b"event: content_block_stop\ndata: ");
                                        out.extend_from_slice(
                                            serde_json::to_string(&stop).unwrap_or_else(|_| r#"{"type":"error","error":{"type":"internal_error","message":"Internal server error"}}"#.to_string()).as_bytes(),
                                        );
                                        out.extend_from_slice(b"\n\n");
                                    }

                                    let anthropic_stop = match finish_reason.as_str() {
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
                                    out.extend_from_slice(b"event: message_delta\ndata: ");
                                    out.extend_from_slice(
                                        serde_json::to_string(&msg_delta).unwrap_or_else(|_| r#"{"type":"error","error":{"type":"internal_error","message":"Internal server error"}}"#.to_string()).as_bytes(),
                                    );
                                    out.extend_from_slice(b"\n\n");

                                    out.extend_from_slice(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
                                }
                            }

                            return Poll::Ready(Some(Ok(out.freeze())));
                        }
                    }

                    if s.starts_with("event: error") || s.starts_with(": keep-alive") {
                        return Poll::Ready(Some(Ok(chunk)));
                    }
                    // Skip chunk and poll next
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAISseProbe {
    pub choices: Option<Vec<OpenAIChoiceProbe>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAIChoiceProbe {
    pub delta: Option<OpenAIDeltaProbe>,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAIDeltaProbe {
    pub content: Option<String>,
    pub tool_calls: Option<Vec<OpenAIToolCallProbe>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAIToolCallProbe {
    pub id: Option<String>,
    pub function: Option<OpenAIFunctionCallProbe>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAIFunctionCallProbe {
    pub name: Option<String>,
    pub arguments: Option<String>,
}
