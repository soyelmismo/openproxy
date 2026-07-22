use crate::translation::anthropic::*;
use crate::translation::types::*;
use openproxy_types::error::{CoreError, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

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

        AnthropicSseEvent::MessageDelta { delta, .. } => {
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

            let chunk = serde_json::json!({
                "id": chunk_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [choice],
            });
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
    let trimmed = line.trim_end_matches(['\r', '\n']);

    // Empty / comment lines are ignored.
    if trimmed.is_empty() || trimmed.starts_with(':') {
        return Ok(None);
    }

    // We're only interested in the `data:` field. Other SSE fields (event:, id:, ...)
    // are accepted for forward compatibility but ignored here.
    let payload = match trimmed.strip_prefix("data:") {
        Some(rest) => rest.trim_start(),
        None => return Ok(None),
    };

    if payload.is_empty() {
        return Ok(None);
    }

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

    let event: AnthropicSseEvent = serde_json::from_value(probe)
        .map_err(|e| CoreError::Parse(format!("invalid Anthropic SSE event: {e}")))?;

    Ok(Some(event))
}

fn format_sse_data(payload: &serde_json::Value) -> String {
    format!("data: {}\n\n", payload)
}
