//! Streaming response body accumulator.
//!
//! Gathers chunks received during a streaming upstream turn and assembles
//! a single OpenAI-style `chat.completion` JSON value at the end, so the
//! persisted `usage.response_body_json` column is non-NULL for streaming
//! rows (matching the non-streaming behavior).
//!
//! Spec: docs/specs/gate-G1-streaming-response-body-persistence.md
//!
//! The accumulator is constructed only when `Pipeline::is_recording() == true`,
//! so when recording is OFF the only cost is a single bool check at function
//! entry. The OpenAI fast path (H6) — which avoids JSON parsing for chunks
//! that carry no `usage` or `finish_reason` — is preserved: the accumulator
//! stores the raw chunk payloads and parses them only at `finish()`.
//!
//! Cap: `MAX_ACCUMULATED_BYTES = 16 MiB`. When the accumulated text would
//! exceed this, `truncated` is set to `true` and the JSON's `extra` map
//! carries `{"truncated": true}`. This bounds heap usage under high
//! concurrency (proxy handles many in-flight streams; each could in
//! principle grow to the upstream 32 MiB cap).

use serde_json::{json, Map, Value};

use crate::translation::OpenAIUsage;

/// Extract `delta.content` from an OpenAI streaming chunk JSON payload
/// WITHOUT full JSON parsing. Finds `"content":"` and extracts the string
/// value by scanning for the closing `"`, correctly handling JSON escape
/// sequences. This is ~50-100x faster than `serde_json::from_str::<Value>`
/// because it avoids allocating the full AST.
///
/// Returns `None` when the payload has no `delta.content` (empty deltas,
/// tool-call-only chunks, role-only chunks, etc.).
fn extract_delta_content(payload: &str) -> Option<&str> {
    let marker = b"\"content\":\"";
    let bytes = payload.as_bytes();
    let pos = memchr::memmem::find(bytes, marker)?;
    let value_start = pos + marker.len();

    // Scan forward for the closing quote, handling JSON escape sequences
    let mut i = value_start;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2; // skip escaped char and its following byte
            continue;
        }
        if bytes[i] == b'"' {
            // SAFETY: marker is ASCII; the span between quotes is valid
            // UTF-8 because it came from a valid JSON string.
            return Some(&payload[value_start..i]);
        }
        i += 1;
    }
    None
}

/// Maximum number of bytes the accumulator's text fields may collectively
/// hold. After this is reached, additional chunks are dropped and the
/// `truncated` flag is set. The upstream `http_body_util::Limited` cap
/// (32 MiB in `upstream/client.rs:541`) is the authoritative bound; this
/// 16 MiB secondary cap exists to bound the per-stream heap footprint of
/// the accumulator itself under high concurrency.
pub const MAX_ACCUMULATED_BYTES: usize = 16 * 1024 * 1024;

/// Per-provider marker for tool_use events. Anthropic streams a tool call
/// across multiple SSE events; this enum lets the loop dispatch without
/// inspecting the raw payload.
#[derive(Debug, Clone)]
pub enum AnthropicToolEvent {
    /// `content_block_start` with `type: "tool_use"`. Carries `id` and
    /// `name`. The accumulator opens a new tool_call entry.
    Open { id: String, name: String },
    /// `content_block_delta` with `type: "input_json_delta"`. Carries a
    /// `partial_json` fragment that gets appended to the in-flight tool
    /// call's `arguments`.
    Delta { partial_json: String },
    /// `content_block_stop`. Closes the in-flight tool call.
    Close,
}

/// A single accumulated tool call (Anthropic or OpenAI). For OpenAI the
/// `arguments` field is a JSON-encoded string per the OpenAI spec. For
/// Anthropic it's the concatenation of `partial_json` fragments.
#[derive(Debug, Clone)]
pub struct AccumulatedToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Provider-agnostic accumulator that the streaming loop in
/// `pipeline.rs::dispatch_upstream_streaming` owns. Construct only when
/// `Pipeline::is_recording() == true`.
pub struct ResponseAccumulator {
    /// Concatenated `delta.content` extracted incrementally from each
    /// chunk during `append_openai_raw`. No JSON parsing is done at
    /// `finish()` — the content is already assembled.
    content: String,
    /// Concatenated reasoning content (o1, deepseek-r1, kimi-k2-thinking
    /// for OpenAI; extended thinking for Anthropic; thought parts for
    /// Gemini). `None` if no reasoning was ever emitted.
    reasoning: Option<String>,
    /// Accumulated tool calls. For OpenAI, populated from
    /// `delta.tool_calls[]` on each chunk. For Anthropic, populated via
    /// `update_anthropic_tool_use` (the existing `AnthropicToolUseAccumulator`
    /// in `sse.rs` is cleared on `content_block_stop` and cannot be relied
    /// upon after the fact).
    tool_calls: Vec<AccumulatedToolCall>,
    /// Inherited from the existing `usage` local in the loop.
    usage: Option<OpenAIUsage>,
    /// Inherited from the existing `stop_reason` local.
    stop_reason: Option<String>,
    /// Total bytes currently held in `content_parts` + `reasoning`.
    total_bytes: usize,
    /// True if `MAX_ACCUMULATED_BYTES` was reached and further content
    /// was dropped. Surfaces in the final JSON's `extra` map.
    truncated: bool,
}

impl ResponseAccumulator {
    pub fn new() -> Self {
        Self {
            content: String::new(),
            reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
            stop_reason: None,
            total_bytes: 0,
            truncated: false,
        }
    }

    /// Append an OpenAI-format raw payload string (e.g. the JSON inside
    /// `data: {...}`). Extracts `delta.content` incrementally using a
    /// lightweight string scan (~50-100x faster than a full JSON parse).
    /// No JSON parsing is done at `finish()` — the content is already
    /// assembled.
    pub fn append_openai_raw(&mut self, payload: &str) {
        if self.truncated {
            return;
        }
        if let Some(content) = extract_delta_content(payload) {
            let additional = content.len();
            if self.total_bytes + additional > MAX_ACCUMULATED_BYTES {
                self.truncated = true;
                return;
            }
            self.content.push_str(content);
            self.total_bytes += additional;
        }
    }

    /// Append a string to the reasoning accumulator. Used for o1-style
    /// reasoning_content (OpenAI), thinking_delta (Anthropic), and
    /// thought:true parts (Gemini).
    pub fn append_reasoning(&mut self, text: &str) {
        if self.truncated || text.is_empty() {
            return;
        }
        let additional = text.len();
        if self.total_bytes + additional > MAX_ACCUMULATED_BYTES {
            self.truncated = true;
            return;
        }
        self.reasoning = Some(match self.reasoning.take() {
            Some(existing) => {
                let mut combined = existing;
                combined.push_str(text);
                combined
            }
            None => text.to_string(),
        });
        self.total_bytes += additional;
    }

    /// Record the final usage (replaces any prior value). Usually the
    /// last chunk carries it.
    pub fn set_usage(&mut self, usage: OpenAIUsage) {
        self.usage = Some(usage);
    }

    /// Record the first non-null stop_reason. Subsequent non-null values
    /// are ignored (matches the existing `stop_reason` local in
    /// `dispatch_upstream_streaming`).
    pub fn set_stop_reason(&mut self, reason: String) {
        if self.stop_reason.is_none() {
            self.stop_reason = Some(reason);
        }
    }

    /// Append a tool call from OpenAI's `delta.tool_calls[]`. The OpenAI
    /// wire format already gives the call as a single chunk; the only
    /// reason we accumulate is so the persisted `response_body_json`
    /// carries a clean tool_calls array (not the streaming deltas).
    pub fn append_openai_tool_call(
        &mut self,
        id: Option<String>,
        name: String,
        arguments: String,
    ) {
        self.tool_calls.push(AccumulatedToolCall {
            id: id.unwrap_or_default(),
            name,
            arguments,
        });
    }

    /// Anthropic tool_use event handler. Called from the streaming loop
    /// at `pipeline.rs:2692-2699` (alongside the existing
    /// `tool_use_acc` threading). Owns its own state to survive the
    /// `content_block_stop` clear in `translate_anthropic_sse_event`.
    pub fn update_anthropic_tool_use(&mut self, event: AnthropicToolEvent) {
        match event {
            AnthropicToolEvent::Open { id, name } => {
                self.tool_calls.push(AccumulatedToolCall {
                    id,
                    name,
                    arguments: String::new(),
                });
            }
            AnthropicToolEvent::Delta { partial_json } => {
                if let Some(last) = self.tool_calls.last_mut() {
                    last.arguments.push_str(&partial_json);
                }
            }
            AnthropicToolEvent::Close => {
                // Nothing to do — the in-flight entry is already in
                // self.tool_calls. Subsequent `Open` events push a new
                // entry, so multi-tool-call streams work correctly.
            }
        }
    }

    /// True if any content was accumulated.
    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
            && self.reasoning.is_none()
            && self.tool_calls.is_empty()
    }

    /// True if `MAX_ACCUMULATED_BYTES` was reached.
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Build the final OpenAI-style response JSON value. The shape
    /// round-trips through `OpenAIResponse` (translation.rs:80-89):
    /// `reasoning_content` and `tool_calls` go into `message.extra`
    /// (the `#[serde(flatten)]` catch-all on `OpenAIMessage`).
    pub fn finish(&self, chunk_id: &str, created: u64, model: &str) -> Value {
        // Content is already assembled incrementally in `append_openai_raw`
        // — no JSON re-parsing needed. This bounded by MAX_ACCUMULATED_BYTES
        // (16 MiB) and runs ONCE at the end of the stream — not per chunk.
        let content = &self.content;

        // Build the message object. `reasoning_content` and `tool_calls`
        // go into `extra` (the flatten catch-all) because `OpenAIMessage`
        // has no typed fields for them.
        let mut message = Map::new();
        message.insert("role".to_string(), Value::String("assistant".to_string()));
        if !content.is_empty() {
            message.insert("content".to_string(), Value::String(content.clone()));
        } else {
            message.insert("content".to_string(), Value::Null);
        }
        let mut extra = Map::new();
        if let Some(reasoning) = &self.reasoning {
            extra.insert(
                "reasoning_content".to_string(),
                Value::String(reasoning.clone()),
            );
        }
        if !self.tool_calls.is_empty() {
            let tool_calls_value: Vec<Value> = self
                .tool_calls
                .iter()
                .map(|tc| {
                    json!({
                        "id": tc.id,
                        "type": "function",
                        "function": {
                            "name": tc.name,
                            "arguments": tc.arguments,
                        }
                    })
                })
                .collect();
            extra.insert("tool_calls".to_string(), Value::Array(tool_calls_value));
        }
        if self.truncated {
            extra.insert("truncated".to_string(), Value::Bool(true));
        }

        let mut choice = Map::new();
        choice.insert("index".to_string(), Value::Number(0u64.into()));
        let mut message_with_extra = message;
        for (k, v) in extra {
            message_with_extra.insert(k, v);
        }
        choice.insert("message".to_string(), Value::Object(message_with_extra));
        choice.insert(
            "finish_reason".to_string(),
            self.stop_reason
                .as_ref()
                .map(|s| Value::String(s.clone()))
                .unwrap_or(Value::Null),
        );

        let mut response = Map::new();
        response.insert("id".to_string(), Value::String(chunk_id.to_string()));
        response.insert(
            "object".to_string(),
            Value::String("chat.completion".to_string()),
        );
        response.insert("created".to_string(), Value::Number(created.into()));
        response.insert("model".to_string(), Value::String(model.to_string()));
        response.insert("choices".to_string(), Value::Array(vec![Value::Object(choice)]));
        if let Some(usage) = &self.usage {
            response.insert(
                "usage".to_string(),
                json!({
                    "prompt_tokens": usage.prompt_tokens,
                    "completion_tokens": usage.completion_tokens,
                    "total_tokens": usage.total_tokens,
                }),
            );
        }
        Value::Object(response)
    }
}

impl Default for ResponseAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_accumulator_produces_minimal_response() {
        let acc = ResponseAccumulator::new();
        let v = acc.finish("chatcmpl-test", 1234, "test-model");
        assert_eq!(v["id"], "chatcmpl-test");
        assert_eq!(v["model"], "test-model");
        assert_eq!(v["choices"][0]["message"]["role"], "assistant");
        assert_eq!(v["choices"][0]["message"]["content"], Value::Null);
        assert_eq!(v["choices"][0]["finish_reason"], Value::Null);
        assert!(v.get("usage").is_none());
    }

    #[test]
    fn openai_raw_payloads_concatenate_content() {
        let mut acc = ResponseAccumulator::new();
        acc.append_openai_raw(r#"{"choices":[{"delta":{"content":"hi"}}]}"#);
        acc.append_openai_raw(r#"{"choices":[{"delta":{"content":" there"}}]}"#);
        let v = acc.finish("id", 0, "m");
        assert_eq!(v["choices"][0]["message"]["content"], "hi there");
    }

    #[test]
    fn reasoning_goes_into_extra() {
        let mut acc = ResponseAccumulator::new();
        acc.append_reasoning("step 1");
        acc.append_reasoning(" + step 2");
        let v = acc.finish("id", 0, "m");
        // reasoning_content is in `extra` (the flatten catch-all)
        // — round-trips through OpenAIMessage
        let msg = &v["choices"][0]["message"];
        assert!(msg.get("reasoning_content").is_some());
        assert_eq!(msg["reasoning_content"], "step 1 + step 2");
    }

    #[test]
    fn anthropic_tool_use_lifecycle() {
        let mut acc = ResponseAccumulator::new();
        acc.update_anthropic_tool_use(AnthropicToolEvent::Open {
            id: "toolu_1".to_string(),
            name: "get_weather".to_string(),
        });
        acc.update_anthropic_tool_use(AnthropicToolEvent::Delta {
            partial_json: r#"{"city":"#.to_string(),
        });
        acc.update_anthropic_tool_use(AnthropicToolEvent::Delta {
            partial_json: r#""Madrid"}"#.to_string(),
        });
        acc.update_anthropic_tool_use(AnthropicToolEvent::Close);
        let v = acc.finish("id", 0, "m");
        let tool_calls = v["choices"][0]["message"]["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "toolu_1");
        assert_eq!(tool_calls[0]["function"]["name"], "get_weather");
        assert_eq!(tool_calls[0]["function"]["arguments"], r#"{"city":"Madrid"}"#);
    }

    #[test]
    fn cap_truncates_and_sets_flag() {
        let mut acc = ResponseAccumulator::new();
        // Push a payload whose extracted content is exactly at the cap.
        let big_content = "x".repeat(MAX_ACCUMULATED_BYTES);
        let payload = format!(
            r#"{{"choices":[{{"index":0,"delta":{{"content":"{}"}},"finish_reason":null}}]}}"#,
            big_content
        );
        acc.append_openai_raw(&payload);
        assert!(!acc.is_truncated());
        // One more chunk pushes it over the cap.
        acc.append_openai_raw(r#"{"choices":[{"index":0,"delta":{"content":"more"},"finish_reason":null}]}"#);
        assert!(acc.is_truncated());
        let v = acc.finish("id", 0, "m");
        assert_eq!(v["choices"][0]["message"]["truncated"], Value::Bool(true));
    }

    #[test]
    fn usage_and_stop_reason_populated() {
        let mut acc = ResponseAccumulator::new();
        acc.set_usage(OpenAIUsage {
            prompt_tokens: 10,
            completion_tokens: 20,
            total_tokens: 30,
        });
        acc.set_stop_reason("stop".to_string());
        let v = acc.finish("id", 0, "m");
        assert_eq!(v["usage"]["prompt_tokens"], 10);
        assert_eq!(v["usage"]["completion_tokens"], 20);
        assert_eq!(v["usage"]["total_tokens"], 30);
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
    }
}
