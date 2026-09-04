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
//! Cap: `MAX_ACCUMULATED_BYTES = 4 MiB`. When the accumulated text would
//! exceed this, `truncated` is set to `true` and the JSON's `extra` map
//! carries `{"truncated": true}`. This bounds heap usage under high
//! concurrency (50 concurrent streams × 4 MiB = 200 MiB worst case).
//! Previously 16 MiB (800 MiB worst case at 50 streams); reduced for
//! RAM optimization. The upstream `http_body_util::Limited` cap is the
//! authoritative bound; this secondary cap exists to bound the
//! per-stream heap footprint of the accumulator itself.

use serde_json::{Map, Value, json};

use crate::translation::OpenAIUsage;

/// Scan `payload` for `marker` (a JSON `"field":"` literal) and return the
/// raw substring between the opening and closing quotes, honouring `\` escapes.
/// Zero allocation; `marker` must be a static byte slice for the hot path.
fn extract_json_string_field<'a>(payload: &'a str, marker: &[u8]) -> Option<&'a str> {
    let bytes = payload.as_bytes();
    let pos = memchr::memmem::find(bytes, marker)?;
    let value_start = pos + marker.len();

    // Scan forward for the closing quote, handling JSON escape sequences.
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

/// Extract `delta.content` from an OpenAI streaming chunk JSON payload
/// WITHOUT full JSON parsing. Finds `"content":"` and extracts the string
/// value by scanning for the closing `"`, correctly handling JSON escape
/// sequences. This is ~50-100x faster than `serde_json::from_str::<Value>`
/// because it avoids allocating the full AST.
///
/// Returns `None` when the payload has no `delta.content` (empty deltas,
/// tool-call-only chunks, role-only chunks, etc.).
fn extract_delta_content(payload: &str) -> Option<&str> {
    extract_json_string_field(payload, b"\"content\":\"")
}

/// Extract `delta.reasoning_content` from an OpenAI streaming chunk JSON
/// payload. Uses the same lightweight string scan as `extract_delta_content`.
/// Returns `None` when no `reasoning_content` field is present.
pub fn extract_reasoning_content(payload: &str) -> Option<&str> {
    extract_json_string_field(payload, b"\"reasoning_content\":\"")
}

/// Normalize non-standard reasoning fields in an OpenAI streaming chunk.
///
/// Some providers (e.g. nex-agi via OpenRouter) send reasoning using
/// non-standard field names:
/// - `delta.reasoning` (string) instead of `delta.reasoning_content`
/// - `delta.reasoning_details[]` (array of `{type, text, index}`) instead
///
/// This function translates these to the standard `delta.reasoning_content`
/// format and strips the non-standard fields, so clients that expect the
/// OpenAI SDK shape (OpenCode, continue.dev, etc.) don't get confused.
///
/// Returns `Some(normalized_json)` when the payload contains non-standard
/// reasoning fields. Returns `None` when the payload is already clean
/// (no change needed), avoiding an allocation on the fast path.
fn should_check_reasoning_fields(payload: &str) -> bool {
    if !payload.contains("reasoning") {
        return false;
    }
    (payload.contains("\"reasoning\":") && !payload.contains("\"reasoning_content\":"))
        || payload.contains("\"reasoning_details\":")
}

fn convert_reasoning_field(obj: &mut serde_json::Map<String, Value>) -> bool {
    let Some(reasoning) = obj.remove("reasoning") else {
        return false;
    };
    if let Some(text) = reasoning.as_str()
        && !text.is_empty()
        && !obj.contains_key("reasoning_content")
    {
        obj.insert(
            "reasoning_content".to_string(),
            serde_json::Value::String(text.to_string()),
        );
    }
    true
}

fn merge_reasoning_details(obj: &mut serde_json::Map<String, Value>, details: serde_json::Value) {
    let Some(arr) = details.as_array() else {
        return;
    };
    let combined: String = arr
        .iter()
        .filter_map(|d| d.get("text").and_then(|t| t.as_str()))
        .collect();
    if combined.is_empty() {
        return;
    }
    if let Some(serde_json::Value::String(existing_str)) = obj.get_mut("reasoning_content") {
        existing_str.push_str(&combined);
    } else {
        obj.insert(
            "reasoning_content".to_string(),
            serde_json::Value::String(combined),
        );
    }
}

fn apply_reasoning_normalizations(obj: &mut serde_json::Map<String, Value>) {
    let reasoning_was_present = convert_reasoning_field(obj);
    #[allow(clippy::collapsible_if)]
    if let Some(details) = obj.remove("reasoning_details") {
        if !reasoning_was_present {
            merge_reasoning_details(obj, details);
        }
    }
}

pub fn normalize_nonstandard_reasoning_fields(payload: &str) -> Option<String> {
    if !should_check_reasoning_fields(payload) {
        return None;
    }

    let mut v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let choices = v.get_mut("choices")?.as_array_mut()?;
    let choice = choices.first_mut()?;
    let delta = choice.get_mut("delta")?;
    let obj = delta.as_object_mut()?;

    apply_reasoning_normalizations(obj);
    serde_json::to_string(&v).ok()
}

/// Maximum number of bytes the accumulator's text fields may collectively
/// hold. After this is reached, additional chunks are dropped and the
/// `truncated` flag is set. The upstream `http_body_util::Limited` cap
/// (8 MiB in `upstream/client.rs:585`) is the authoritative bound; this
/// 4 MiB secondary cap exists to bound the per-stream heap footprint of
/// the accumulator itself under high concurrency. (Was 16 MiB — reduced
/// for RAM optimization: 50 concurrent streams × 16 MiB = 800 MiB worst
/// case; 4 MiB × 50 = 200 MiB, a 4x reduction.)
pub const MAX_ACCUMULATED_BYTES: usize = 4 * 1024 * 1024;

/// Data for opening an Anthropic tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicToolOpen {
    pub id: String,
    pub name: String,
}

/// Per-provider marker for tool_use events. Anthropic streams a tool call
/// across multiple SSE events; this enum lets the loop dispatch without
/// inspecting the raw payload.
#[derive(Debug, Clone)]
pub enum AnthropicToolEvent {
    /// `content_block_start` with `type: "tool_use"`. Carries `id` and
    /// `name`. The accumulator opens a new tool_call entry.
    Open(Box<AnthropicToolOpen>),
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
#[derive(Debug, Clone, Default)]
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
    content: Vec<u8>,
    /// Concatenated reasoning content (o1, deepseek-r1, kimi-k2-thinking
    /// for OpenAI; extended thinking for Anthropic; thought parts for
    /// Gemini). `None` if no reasoning was ever emitted.
    reasoning: Option<Vec<u8>>,
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
    /// True when the stream was interrupted (client disconnect, race
    /// lost, sink error, etc.) before reaching `[DONE]`. Set by the
    /// pipeline's failure helpers before calling `finish()` so the
    /// persisted JSON carries a `"partial": true` marker in its
    /// `extra` map. The dashboard reads this to show a "Partial
    /// response — stream was interrupted" banner in the Response tab.
    partial: bool,
    /// Raw response stream lines (including non-JSON content or error responses)
    /// captured incrementally up to a max size (e.g. 32 KiB) for debugging.
    raw_response_body: Vec<u8>,
}

impl ResponseAccumulator {
    /// Public accessor for the accumulated content text. Used by the
    /// token estimator to estimate completion tokens when the upstream
    /// didn't report usage.
    pub fn content_text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.content)
    }

    pub fn new() -> Self {
        Self {
            content: Vec::new(),
            reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
            stop_reason: None,
            total_bytes: 0,
            truncated: false,
            partial: false,
            raw_response_body: Vec::new(),
        }
    }

    /// Append a raw stream line as read from the upstream connection (for debugging
    /// empty/interrupted streams). Caps at 32 KiB to limit memory overhead.
    pub fn append_raw_line(&mut self, line: &str) {
        if self.raw_response_body.len() < 32768 {
            let limit = 32768 - self.raw_response_body.len();
            let to_add = line.as_bytes();
            let to_add = &to_add[..to_add.len().min(limit)];
            self.raw_response_body.extend_from_slice(to_add);
            if to_add.len() < line.len() {
                self.raw_response_body.extend_from_slice(b"... [truncated]");
            } else {
                self.raw_response_body.push(b'\n');
            }
        }
    }

    /// Mark this accumulator as representing a partial (interrupted)
    /// stream. The pipeline's streaming failure helpers call this
    /// before `finish()` so the persisted JSON carries
    /// `"partial": true` in its `extra` map. The dashboard reads
    /// that marker to show a "Partial response — stream was
    /// interrupted" banner in the Response tab, so the operator
    /// knows the response didn't complete normally even though
    /// there IS a response body to inspect.
    pub fn mark_partial(&mut self) {
        self.partial = true;
    }

    /// True if this accumulator represents a partial (interrupted)
    /// stream. Equivalent to checking the `partial` field directly.
    pub fn is_partial(&self) -> bool {
        self.partial
    }

    /// Checks if all accumulated fields (including raw stream logs) are empty.
    pub fn is_completely_empty(&self) -> bool {
        self.content.is_empty()
            && self.reasoning.is_none()
            && self.tool_calls.is_empty()
            && self.raw_response_body.is_empty()
    }

    /// Public accessor for the accumulated raw response body. Used by
    /// the pipeline's failure handlers to inspect whether the upstream
    /// sent an inline error (e.g. OpenRouter's 502/provider_unavailable
    /// inside an SSE data chunk) so the error message can reflect the
    /// actual upstream error instead of a generic "client disconnected".
    pub fn raw_response_body(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.raw_response_body)
    }
}

#[derive(serde::Deserialize)]
struct ToolCallProbeOuter<'a> {
    #[serde(borrow)]
    choices: Option<Vec<ToolCallProbeChoice<'a>>>,
}
#[derive(serde::Deserialize)]
struct ToolCallProbeChoice<'a> {
    #[serde(borrow)]
    delta: Option<ToolCallProbeDelta<'a>>,
}
#[derive(serde::Deserialize)]
struct ToolCallProbeDelta<'a> {
    #[serde(borrow)]
    tool_calls: Option<Vec<ToolCallProbe<'a>>>,
}
#[derive(serde::Deserialize)]
struct ToolCallProbe<'a> {
    index: Option<usize>,
    #[serde(borrow)]
    id: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    function: Option<ToolCallFunctionProbe<'a>>,
}
#[derive(serde::Deserialize)]
struct ToolCallFunctionProbe<'a> {
    #[serde(borrow)]
    name: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    arguments: Option<std::borrow::Cow<'a, str>>,
}

fn parse_tool_call_probe(payload: &str) -> Option<Vec<ToolCallProbe<'_>>> {
    if !payload.contains("\"tool_calls\"") {
        return None;
    }
    let v = serde_json::from_slice::<ToolCallProbeOuter<'_>>(payload.as_bytes()).ok()?;
    v.choices
        .and_then(|c| c.into_iter().next())
        .and_then(|c| c.delta)
        .and_then(|d| d.tool_calls)
}

fn parse_upstream_error_payload(json_bytes: &[u8]) -> Option<(u16, String)> {
    #[derive(serde::Deserialize)]
    struct UpstreamErrorProbe<'a> {
        choices: Option<Vec<serde_json::Value>>,
        #[serde(borrow)]
        error: Option<ErrorObjProbe<'a>>,
    }
    #[derive(serde::Deserialize)]
    struct ErrorObjProbe<'a> {
        code: Option<u64>,
        #[serde(borrow)]
        message: Option<std::borrow::Cow<'a, str>>,
    }
    let v = serde_json::from_slice::<UpstreamErrorProbe<'_>>(json_bytes).ok()?;
    if !v.choices.is_none_or(|c| c.is_empty()) {
        return None;
    }
    let error_obj = v.error?;
    let code = error_obj.code.unwrap_or(502) as u16;
    let message = error_obj
        .message
        .as_deref()
        .unwrap_or("unknown upstream error in SSE stream")
        .to_string();
    Some((code, message))
}

fn extract_error_from_line(line: &[u8]) -> Option<(u16, String)> {
    let json_bytes = line
        .strip_prefix(b"data: ")
        .or_else(|| line.strip_prefix(b"data:"))
        .unwrap_or(line);

    let json_bytes = json_bytes.trim_ascii();

    if !json_bytes.starts_with(b"{") {
        return None;
    }
    parse_upstream_error_payload(json_bytes)
}

impl ResponseAccumulator {
    pub fn extract_upstream_error_from_raw(&self) -> Option<(u16, String)> {
        if !self
            .raw_response_body
            .windows(8)
            .any(|w| w == b"\"error\":")
        {
            return None;
        }
        self.raw_response_body
            .split(|&b| b == b'\n')
            .find_map(|line| extract_error_from_line(line))
    }

    fn append_delta_content_if_present(&mut self, payload: &str) {
        let Some(content) = extract_delta_content(payload) else {
            return;
        };
        let additional = content.len();
        if self.total_bytes + additional > MAX_ACCUMULATED_BYTES {
            self.truncated = true;
            return;
        }
        self.content.extend_from_slice(content.as_bytes());
        self.total_bytes += additional;
    }

    fn append_delta_tool_calls_if_present(&mut self, payload: &str) {
        let Some(tool_calls) = parse_tool_call_probe(payload) else {
            return;
        };

        for tc in tool_calls {
            let index = tc.index.unwrap_or(0);
            let id = tc.id.as_deref();
            let name = tc.function.as_ref().and_then(|f| f.name.as_deref());
            let arguments = tc.function.as_ref().and_then(|f| f.arguments.as_deref());
            self.update_openai_tool_call_delta(index, id, name, arguments);
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
        self.append_delta_content_if_present(payload);
        self.append_delta_tool_calls_if_present(payload);
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
        self.reasoning
            .get_or_insert_with(Vec::new)
            .extend_from_slice(text.as_bytes());
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
    pub fn set_stop_reason(&mut self, reason: &str) {
        if self.stop_reason.is_none() {
            self.stop_reason = Some(reason.to_string());
        }
    }

    /// Update an OpenAI-format tool call delta at `index`. If `id` or
    /// `name` are present, they are set. `arguments` are appended to
    /// any existing arguments for that tool call index.
    pub fn update_openai_tool_call_delta(
        &mut self,
        index: usize,
        id: Option<&str>,
        name: Option<&str>,
        arguments: Option<&str>,
    ) {
        while self.tool_calls.len() <= index {
            self.tool_calls.push(AccumulatedToolCall::default());
        }
        let tc = &mut self.tool_calls[index];
        if let Some(id) = id {
            tc.id = id.to_string();
        }
        if let Some(name) = name {
            tc.name = name.to_string();
        }
        if let Some(args) = arguments {
            let additional = args.len();
            if self.total_bytes + additional > MAX_ACCUMULATED_BYTES {
                self.truncated = true;
                return;
            }
            tc.arguments.push_str(args);
            self.total_bytes += additional;
        }
    }

    /// Append a tool call from OpenAI's `delta.tool_calls[]`. The OpenAI
    /// wire format already gives the call as a single chunk; the only
    /// reason we accumulate is so the persisted `response_body_json`
    /// carries a clean tool_calls array (not the streaming deltas).
    pub fn append_openai_tool_call(&mut self, id: Option<&str>, name: &str, arguments: &str) {
        self.update_openai_tool_call_delta(0, id, Some(name), Some(arguments));
    }

    /// Anthropic tool_use event handler. Called from the streaming loop
    /// at `pipeline.rs:2692-2699` (alongside the existing
    /// `tool_use_acc` threading). Owns its own state to survive the
    /// `content_block_stop` clear in `translate_anthropic_sse_event`.
    pub fn update_anthropic_tool_use(&mut self, event: AnthropicToolEvent) {
        match event {
            AnthropicToolEvent::Open(open) => {
                self.tool_calls.push(AccumulatedToolCall {
                    id: open.id,
                    name: open.name,
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
        self.content.is_empty() && self.reasoning.is_none() && self.tool_calls.is_empty()
    }

    /// True if `MAX_ACCUMULATED_BYTES` was reached.
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    fn build_finish_extra(&self) -> Map<String, Value> {
        let mut extra = Map::new();
        if let Some(reasoning) = &self.reasoning {
            extra.insert(
                "reasoning_content".to_string(),
                Value::String(String::from_utf8_lossy(reasoning).into_owned()),
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
        if self.partial {
            extra.insert("partial".to_string(), Value::Bool(true));
        }
        if !self.raw_response_body.is_empty() {
            extra.insert(
                "raw_response_body".to_string(),
                Value::String(String::from_utf8_lossy(&self.raw_response_body).into_owned()),
            );
        }
        extra
    }

    fn build_finish_message(&self) -> Map<String, Value> {
        let mut message = Map::new();
        message.insert("role".to_string(), Value::String("assistant".to_string()));
        let content_val = if self.content.is_empty() {
            Value::Null
        } else {
            Value::String(String::from_utf8_lossy(&self.content).into_owned())
        };
        message.insert("content".to_string(), content_val);
        for (k, v) in self.build_finish_extra() {
            message.insert(k, v);
        }
        message
    }

    fn build_finish_choice(&self) -> Map<String, Value> {
        let mut choice = Map::new();
        choice.insert("index".to_string(), Value::Number(0u64.into()));
        choice.insert(
            "message".to_string(),
            Value::Object(self.build_finish_message()),
        );
        choice.insert(
            "finish_reason".to_string(),
            self.stop_reason
                .as_ref()
                .map_or(Value::Null, |s| Value::String(s.to_owned())),
        );
        choice
    }

    /// Build the final OpenAI-style response JSON value. The shape
    /// round-trips through `OpenAIResponse` (translation.rs:80-89):
    /// `reasoning_content` and `tool_calls` go into `message.extra`
    /// (the `#[serde(flatten)]` catch-all on `OpenAIMessage`).
    pub fn finish(&self, chunk_id: &str, created: u64, model: &str) -> Value {
        let mut response = Map::new();
        response.insert("id".to_string(), Value::String(chunk_id.to_string()));
        response.insert(
            "object".to_string(),
            Value::String("chat.completion".to_string()),
        );
        response.insert("created".to_string(), Value::Number(created.into()));
        response.insert("model".to_string(), Value::String(model.to_string()));
        response.insert(
            "choices".to_string(),
            Value::Array(vec![Value::Object(self.build_finish_choice())]),
        );
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

impl crate::streaming::StreamingChunkStage for ResponseAccumulator {
    fn process_chunk(&mut self, payload: &str) -> crate::streaming::StreamAction {
        self.append_openai_raw(payload);
        #[allow(clippy::collapsible_if)]
        if payload.contains("\"reasoning_content\"") {
            #[allow(clippy::collapsible_if)]
            if let Some(rc) = extract_reasoning_content(payload) {
                if !rc.is_empty() {
                    self.append_reasoning(rc);
                }
            }
        }
        crate::streaming::StreamAction::Passthrough
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
    fn openai_raw_payloads_multibyte_utf8_boundaries() {
        let mut acc = ResponseAccumulator::new();
        acc.append_openai_raw(r#"{"choices":[{"delta":{"content":"при"}}]}"#);
        acc.append_openai_raw(r#"{"choices":[{"delta":{"content":"вет"}}]}"#);
        let v = acc.finish("id", 0, "m");
        assert_eq!(v["choices"][0]["message"]["content"], "привет");
    }

    #[test]
    fn openai_raw_payloads_mid_stream_malformed_json() {
        let mut acc = ResponseAccumulator::new();
        acc.append_openai_raw(r#"{"choices":[{"delta":{"content":"good"}}]}"#);
        acc.append_openai_raw(r#"{"choices":[{"delta":{"content":" malformed"#); // malformed
        acc.append_openai_raw(r#"{"choices":[{"delta":{"content":" bye"}}]}"#);
        let v = acc.finish("id", 0, "m");
        assert_eq!(v["choices"][0]["message"]["content"], "good bye");
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
        acc.update_anthropic_tool_use(AnthropicToolEvent::Open(Box::new(AnthropicToolOpen {
            id: "toolu_1".to_string(),
            name: "get_weather".to_string(),
        })));
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
        assert_eq!(
            tool_calls[0]["function"]["arguments"],
            r#"{"city":"Madrid"}"#
        );
    }

    #[test]
    fn cap_truncates_and_sets_flag() {
        let mut acc = ResponseAccumulator::new();
        // Push a payload whose extracted content is exactly at the cap.
        let big_content = "x".repeat(MAX_ACCUMULATED_BYTES);
        let payload = format!(
            r#"{{"choices":[{{"index":0,"delta":{{"content":"{big_content}"}},"finish_reason":null}}]}}"#
        );
        acc.append_openai_raw(&payload);
        assert!(!acc.is_truncated());
        // One more chunk pushes it over the cap.
        acc.append_openai_raw(
            r#"{"choices":[{"index":0,"delta":{"content":"more"},"finish_reason":null}]}"#,
        );
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
            prompt_tokens_details: None,
        });
        acc.set_stop_reason("stop");
        let v = acc.finish("id", 0, "m");
        assert_eq!(v["usage"]["prompt_tokens"], 10);
        assert_eq!(v["usage"]["completion_tokens"], 20);
        assert_eq!(v["usage"]["total_tokens"], 30);
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn partial_flag_sets_marker_in_extra() {
        let mut acc = ResponseAccumulator::new();
        acc.append_openai_raw(r#"{"choices":[{"delta":{"content":"hi"}}]}"#);
        acc.mark_partial();
        assert!(acc.is_partial());
        let v = acc.finish("id", 0, "m");
        assert_eq!(v["choices"][0]["message"]["partial"], Value::Bool(true));
        assert_eq!(v["choices"][0]["message"]["content"], "hi");
    }

    #[test]
    fn content_text_accessor() {
        let mut acc = ResponseAccumulator::new();
        acc.append_openai_raw(r#"{"choices":[{"delta":{"content":"part1"}}]}"#);
        acc.append_openai_raw(r#"{"choices":[{"delta":{"content":" part2"}}]}"#);
        assert_eq!(acc.content_text(), "part1 part2");
    }

    #[test]
    fn append_reasoning_accumulates_correctly() {
        let mut acc = ResponseAccumulator::new();
        acc.append_reasoning("thought 1");
        acc.append_reasoning(" thought 2");
        let v = acc.finish("id", 0, "m");
        assert_eq!(
            v["choices"][0]["message"]["reasoning_content"],
            "thought 1 thought 2"
        );
    }

    #[test]
    fn append_openai_tool_call_accumulates() {
        let mut acc = ResponseAccumulator::new();
        acc.append_openai_tool_call(Some("call_1"), "get_time", r#"{"zone":"UTC"}"#);
        let v = acc.finish("id", 0, "m");
        let tool_calls = v["choices"][0]["message"]["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "call_1");
        assert_eq!(tool_calls[0]["function"]["name"], "get_time");
        assert_eq!(tool_calls[0]["function"]["arguments"], r#"{"zone":"UTC"}"#);
    }

    // ---- Non-standard reasoning normalization ----

    #[test]
    fn normalize_reasoning_field_to_reasoning_content() {
        let payload = r#"{"id":"x","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"content":"","role":"assistant","reasoning":" Need"},"finish_reason":null}]}"#;
        let result = normalize_nonstandard_reasoning_fields(payload);
        assert!(result.is_some(), "should normalize reasoning field");
        let normalized = result.unwrap();
        // Should contain reasoning_content instead of reasoning
        assert!(
            normalized.contains("\"reasoning_content\""),
            "should have reasoning_content: {normalized}"
        );
        assert!(
            !normalized.contains("\"reasoning\":"),
            "should NOT have raw reasoning field: {normalized}"
        );
        // Content should still be present
        assert!(
            normalized.contains("\"content\":\"\""),
            "should preserve content: {normalized}"
        );
        // Parse and verify
        let v: serde_json::Value = serde_json::from_str(&normalized).unwrap();
        let rc = v["choices"][0]["delta"]["reasoning_content"]
            .as_str()
            .unwrap();
        assert_eq!(rc, " Need");
    }

    #[test]
    fn normalize_reasoning_details_array() {
        let payload = r#"{"id":"x","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"content":"","role":"assistant","reasoning_details":[{"type":"reasoning.text","text":"Need","format":"unknown","index":0},{"type":"reasoning.text","text":" to","format":"unknown","index":1}]},"finish_reason":null}]}"#;
        let result = normalize_nonstandard_reasoning_fields(payload);
        assert!(result.is_some(), "should normalize reasoning_details");
        let normalized = result.unwrap();
        let v: serde_json::Value = serde_json::from_str(&normalized).unwrap();
        let rc = v["choices"][0]["delta"]["reasoning_content"]
            .as_str()
            .unwrap();
        assert_eq!(rc, "Need to", "should merge reasoning_details texts");
        assert!(
            v["choices"][0]["delta"].get("reasoning_details").is_none(),
            "should remove reasoning_details"
        );
    }

    #[test]
    fn normalize_standard_reasoning_content_unchanged() {
        let payload = r#"{"id":"x","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"content":"","reasoning_content":"hello"},"finish_reason":null}]}"#;
        let result = normalize_nonstandard_reasoning_fields(payload);
        assert!(
            result.is_none(),
            "standard reasoning_content should not trigger normalization"
        );
    }

    #[test]
    fn normalize_no_reasoning_fields_unchanged() {
        let payload = r#"{"id":"x","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}"#;
        let result = normalize_nonstandard_reasoning_fields(payload);
        assert!(
            result.is_none(),
            "no reasoning fields should not trigger normalization"
        );
    }

    #[test]
    fn extract_reasoning_content_from_normalized() {
        let payload =
            r#"{"choices":[{"delta":{"content":"","reasoning_content":" step by step"}}]}"#;
        let rc = extract_reasoning_content(payload);
        assert_eq!(rc, Some(" step by step"));
    }

    #[test]
    fn extract_reasoning_content_absent() {
        let payload = r#"{"choices":[{"delta":{"content":"hello"}}]}"#;
        let rc = extract_reasoning_content(payload);
        assert!(rc.is_none());
    }

    #[test]
    fn normalize_both_reasoning_and_details() {
        let payload = r#"{"id":"x","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"content":"","role":"assistant","reasoning":"think","reasoning_details":[{"type":"reasoning.text","text":" more","format":"unknown","index":0}]},"finish_reason":null}]}"#;
        let result = normalize_nonstandard_reasoning_fields(payload);
        assert!(result.is_some());
        let v: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        let rc = v["choices"][0]["delta"]["reasoning_content"]
            .as_str()
            .unwrap();
        // `reasoning` wins over `reasoning_details` — only "think",
        // not "think more" (which would be the merge). Providers
        // like NVIDIA send the same text in both fields; merging
        // would duplicate the content.
        assert_eq!(rc, "think");
        assert!(v["choices"][0]["delta"].get("reasoning").is_none());
        // reasoning_details must also be stripped when reasoning
        // was present — it's non-standard and duplicates the text.
        assert!(
            v["choices"][0]["delta"].get("reasoning_details").is_none(),
            "reasoning_details should be stripped even when reasoning is present"
        );
    }

    #[test]
    fn raw_response_body_captured() {
        let mut acc = ResponseAccumulator::new();
        assert!(acc.is_completely_empty());
        acc.append_raw_line("data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}");
        acc.append_raw_line("some raw non-json line");
        assert!(!acc.is_completely_empty());
        let finished = acc.finish("test_chunk_id", 12345, "test_model");
        let raw_body = finished["choices"][0]["message"]["raw_response_body"]
            .as_str()
            .unwrap();
        assert!(raw_body.contains("some raw non-json line"));
        assert!(raw_body.contains("hello"));
    }

    #[test]
    fn raw_response_body_accessor() {
        let mut acc = ResponseAccumulator::new();
        assert!(acc.raw_response_body().is_empty());
        acc.append_raw_line("data: test line");
        assert!(acc.raw_response_body().contains("test line"));
    }

    #[test]
    fn extract_upstream_error_openrouter_502() {
        let mut acc = ResponseAccumulator::new();
        acc.append_raw_line(
            r#"data: {"id":"gen-123","object":"chat.completion.chunk","created":1783260233,"model":"nvidia/nemotron-3-ultra:free","provider":"Nvidia","choices":[],"error":{"code":502,"message":"Upstream error from Nvidia: ResourceExhausted: Worker local total request limit reached (32/32)","metadata":{"error_type":"provider_unavailable"}}}"#,
        );
        let result = acc.extract_upstream_error_from_raw();
        assert!(result.is_some(), "should detect OpenRouter inline error");
        let (code, message) = result.unwrap();
        assert_eq!(code, 502);
        assert!(message.contains("ResourceExhausted"));
        assert!(message.contains("Worker local total request limit"));
    }

    #[test]
    fn extract_upstream_error_no_false_positive_on_normal_chunks() {
        let mut acc = ResponseAccumulator::new();
        // Normal chunk with content — should NOT trigger.
        acc.append_raw_line(r#"data: {"id":"x","choices":[{"delta":{"content":"hello"}}]}"#);
        // Another normal chunk with reasoning.
        acc.append_raw_line(r#"data: {"id":"x","choices":[{"delta":{"reasoning":"think"}}]}"#);
        let result = acc.extract_upstream_error_from_raw();
        assert!(result.is_none(), "should not trigger on normal chunks");
    }

    #[test]
    fn extract_upstream_error_empty_accumulator() {
        let acc = ResponseAccumulator::new();
        assert!(acc.extract_upstream_error_from_raw().is_none());
    }

    #[test]
    fn extract_upstream_error_missing_code_defaults_502() {
        let mut acc = ResponseAccumulator::new();
        acc.append_raw_line(r#"data: {"choices":[],"error":{"message":"Something went wrong"}}"#);
        let result = acc.extract_upstream_error_from_raw();
        assert!(result.is_some());
        let (code, message) = result.unwrap();
        assert_eq!(code, 502, "should default to 502 when code is missing");
        assert_eq!(message, "Something went wrong");
    }
}
