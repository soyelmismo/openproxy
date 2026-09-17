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
//! Streaming responses are passed chunk-by-chunk to the downstream client
//! without an artificial wire-size ceiling, while this accumulator cap
//! bounds the per-stream heap footprint for persistence.
//!
//! Spec: docs/specs/gate-G1-streaming-response-body-persistence.md

use serde_json::{Map, Value, json};

use crate::translation::OpenAIUsage;

/// Decode standard JSON string escape sequences into the destination byte buffer.
pub fn decode_json_escape_into(raw: &str, out: &mut Vec<u8>) {
    let bytes = raw.as_bytes();
    if !bytes.contains(&b'\\') {
        out.extend_from_slice(bytes);
        return;
    }
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'"' => { out.push(b'"'); i += 2; }
                b'\\' => { out.push(b'\\'); i += 2; }
                b'/' => { out.push(b'/'); i += 2; }
                b'b' => { out.push(0x08); i += 2; }
                b'f' => { out.push(0x0C); i += 2; }
                b'n' => { out.push(b'\n'); i += 2; }
                b'r' => { out.push(b'\r'); i += 2; }
                b't' => { out.push(b'\t'); i += 2; }
                b'u' if i + 5 < bytes.len() => {
                    if let Ok(hex_str) = std::str::from_utf8(&bytes[i + 2..i + 6])
                        && let Ok(hex) = u16::from_str_radix(hex_str, 16)
                    {
                        if (0xD800..=0xDBFF).contains(&hex)
                            && i + 11 < bytes.len()
                            && &bytes[i + 6..i + 8] == b"\\u"
                            && let Ok(low_str) = std::str::from_utf8(&bytes[i + 8..i + 12])
                            && let Ok(low) = u16::from_str_radix(low_str, 16)
                            && (0xDC00..=0xDFFF).contains(&low)
                        {
                            let cp = 0x10000 + (((hex as u32 - 0xD800) << 10) | (low as u32 - 0xDC00));
                            if let Some(ch) = char::from_u32(cp) {
                                let mut buf = [0u8; 4];
                                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                                i += 12;
                                continue;
                            }
                        }
                        if let Some(ch) = char::from_u32(hex as u32) {
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                            i += 6;
                            continue;
                        }
                    }
                    out.push(b'\\');
                    i += 1;
                }
                _ => { out.push(b'\\'); i += 1; }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
}

fn find_delta_object(payload: &str) -> Option<usize> {
    let bytes = payload.as_bytes();
    let mut offset = 0;
    while let Some(rel) = memchr::memmem::find(&bytes[offset..], b"\"delta\"") {
        let idx = offset + rel;
        let mut i = idx + 7;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b':' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'{' {
                return Some(i + 1);
            }
        }
        offset = idx + 7;
    }
    None
}

/// Extract a string field from `choices[0].delta` strictly at depth 1,
/// ignoring nested objects and arrays (e.g. `tool_calls`).
fn extract_delta_field<'a>(payload: &'a str, target_key: &str) -> Option<&'a str> {
    let bytes = payload.as_bytes();
    if !payload.contains(target_key) {
        return None;
    }
    let mut i = find_delta_object(payload)?;
    let mut depth: usize = 1;

    while i < bytes.len() && depth > 0 {
        match bytes[i] {
            b'{' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                depth -= 1;
                i += 1;
            }
            b'"' => {
                let str_start = i + 1;
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'"' {
                        break;
                    }
                    i += 1;
                }
                if i >= bytes.len() {
                    return None;
                }
                let key_str = std::str::from_utf8(&bytes[str_start..i]).ok()?;
                i += 1;

                let mut j = i;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b':' {
                    j += 1;
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if depth == 1 && key_str == target_key {
                        if j < bytes.len() && bytes[j] == b'"' {
                            let val_start = j + 1;
                            let mut k = val_start;
                            while k < bytes.len() {
                                if bytes[k] == b'\\' {
                                    k += 2;
                                    continue;
                                }
                                if bytes[k] == b'"' {
                                    return Some(&payload[val_start..k]);
                                }
                                k += 1;
                            }
                        }
                        return None;
                    }
                    i = j;
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    None
}

/// Extract `delta.content` from an OpenAI streaming chunk JSON payload
/// strictly within `choices[0].delta` object boundary, ignoring nested tool calls.
fn extract_delta_content(payload: &str) -> Option<&str> {
    extract_delta_field(payload, "content")
}

/// Extract `delta.reasoning_content` strictly within `choices[0].delta`.
pub fn extract_reasoning_content(payload: &str) -> Option<&str> {
    extract_delta_field(payload, "reasoning_content")
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
    if let Some(details) = obj.remove("reasoning_details")
        && !reasoning_was_present
    {
        merge_reasoning_details(obj, details);
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
/// `truncated` flag is set. Streaming responses are passed chunk-by-chunk to
/// the downstream client without an artificial wire-size ceiling, while this
/// 4 MiB cap exists to bound the per-stream heap footprint of the accumulator
/// itself under high concurrency. (Was 16 MiB — reduced for RAM optimization:
/// 50 concurrent streams × 16 MiB = 800 MiB worst case; 4 MiB × 50 = 200 MiB,
/// a 4x reduction.)
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

fn extract_error_from_line(line: &[u8]) -> Option<(u16, String)> {
    let json_bytes = line
        .strip_prefix(b"data: ")
        .or_else(|| line.strip_prefix(b"data:"))
        .unwrap_or(line)
        .trim_ascii();
    if !json_bytes.starts_with(b"{") {
        return None;
    }
    let json_str = std::str::from_utf8(json_bytes).ok()?;
    let parsed = crate::sse::parse_inline_sse_error(json_str)?;
    Some((parsed.status_code, parsed.message.to_string()))
}

impl ResponseAccumulator {
    pub fn extract_upstream_error_from_raw(&self) -> Option<(u16, String)> {
        if !self.raw_response_body.windows(7).any(|w| w == b"\"error\"") {
            return None;
        }
        self.raw_response_body
            .split(|&b| b == b'\n')
            .find_map(extract_error_from_line)
    }

    fn append_delta_content_if_present(&mut self, payload: &str) {
        let Some(content) = extract_delta_content(payload) else {
            return;
        };
        if self.total_bytes + content.len() > MAX_ACCUMULATED_BYTES {
            self.truncated = true;
            return;
        }
        let prev_len = self.content.len();
        decode_json_escape_into(content, &mut self.content);
        self.total_bytes += self.content.len() - prev_len;
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
        if self.total_bytes + text.len() > MAX_ACCUMULATED_BYTES {
            self.truncated = true;
            return;
        }
        let r = self.reasoning.get_or_insert_with(Vec::new);
        let prev_len = r.len();
        decode_json_escape_into(text, r);
        self.total_bytes += r.len() - prev_len;
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
        if (self.partial || (self.content.is_empty() && self.tool_calls.is_empty()))
            && !self.raw_response_body.is_empty()
        {
            response.insert(
                "raw_response_body".to_string(),
                Value::String(String::from_utf8_lossy(&self.raw_response_body).into_owned()),
            );
        }
        Value::Object(response)
    }
}

impl crate::streaming::StreamingChunkStage for ResponseAccumulator {
    fn process_chunk(&mut self, payload: &str) -> crate::streaming::StreamAction {
        self.append_openai_raw(payload);
        if payload.contains("\"reasoning_content\"")
            && let Some(rc) = extract_reasoning_content(payload)
            && !rc.is_empty()
        {
            self.append_reasoning(rc);
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
#[path = "sse_accumulator_tests.rs"]
mod tests;
