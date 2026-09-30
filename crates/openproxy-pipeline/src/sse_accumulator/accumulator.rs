use serde::Serialize;
use serde_json::Value;

use crate::translation::OpenAIUsage;

use super::parser::{
    decode_json_escape_into, extract_delta_content, extract_error_from_line,
    extract_reasoning_content, parse_tool_call_probe,
};
use super::types::{AccumulatedToolCall, AnthropicToolEvent, MAX_ACCUMULATED_BYTES};

/// Owned by the streaming loop in
/// `pipeline.rs::dispatch_upstream_streaming`, and only constructed when
/// `Pipeline::is_recording() == true`.
pub struct ResponseAccumulator {
    /// Concatenated `delta.content`, extracted incrementally during
    /// `append_openai_raw` so `finish()` does no JSON parsing.
    content: Vec<u8>,
    /// Concatenated reasoning content (o1, deepseek-r1, kimi-k2-thinking
    /// for OpenAI; extended thinking for Anthropic; thought parts for
    /// Gemini). `None` if no reasoning was ever emitted.
    reasoning: Option<Vec<u8>>,
    /// Accumulated tool calls. For OpenAI, populated from
    /// `delta.tool_calls[]` on each chunk. For Anthropic, populated via
    /// `update_anthropic_tool_use`.
    tool_calls: Vec<AccumulatedToolCall>,
    usage: Option<OpenAIUsage>,
    stop_reason: Option<String>,
    /// Total bytes currently held in `content` + `reasoning` + tool call arguments.
    total_bytes: usize,
    /// Set once `MAX_ACCUMULATED_BYTES` is reached, and surfaced in the
    /// final JSON's `extra` map.
    truncated: bool,
    /// Set when the stream was interrupted (client disconnect, race lost,
    /// sink error) before reaching `[DONE]`.
    partial: bool,
    /// Raw stream lines, non-JSON content and error responses included,
    /// captured up to 32 KiB for debugging.
    raw_response_body: Vec<u8>,
}

impl ResponseAccumulator {
    /// Feeds the token estimator when the upstream reported no usage.
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

    /// Appends an upstream stream line verbatim. Caps at 32 KiB.
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

    pub fn mark_partial(&mut self) {
        self.partial = true;
    }

    /// True if this accumulator represents a partial (interrupted) stream.
    pub fn is_partial(&self) -> bool {
        self.partial
    }

    pub fn is_completely_empty(&self) -> bool {
        self.content.is_empty()
            && self.reasoning.is_none()
            && self.tool_calls.is_empty()
            && self.raw_response_body.is_empty()
    }

    pub fn raw_response_body(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.raw_response_body)
    }

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

    /// Appends the JSON inside a `data: {...}` frame. `delta.content` is
    /// extracted by a string scan, an order of magnitude cheaper than a
    /// full JSON parse per chunk.
    pub fn append_openai_raw(&mut self, payload: &str) {
        if self.truncated {
            return;
        }
        self.append_delta_content_if_present(payload);
        self.append_delta_tool_calls_if_present(payload);
    }

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

    pub fn set_usage(&mut self, usage: OpenAIUsage) {
        self.usage = Some(usage);
    }

    pub fn set_stop_reason(&mut self, reason: &str) {
        if self.stop_reason.is_none() {
            self.stop_reason = Some(reason.to_string());
        }
    }

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

    pub fn append_openai_tool_call(&mut self, id: Option<&str>, name: &str, arguments: &str) {
        self.update_openai_tool_call_delta(0, id, Some(name), Some(arguments));
    }

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
            AnthropicToolEvent::Close => {}
        }
    }

    pub fn is_empty(&self) -> bool {
        self.content.is_empty() && self.reasoning.is_none() && self.tool_calls.is_empty()
    }

    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    pub fn finish(&self, chunk_id: &str, created: u64, model: &str) -> Value {
        let finish_reason = match self.stop_reason.as_deref() {
            Some(
                "tool_use" | "tool_call" | "tool_calls" | "toolUse" | "toolCall" | "toolCalls",
            ) => Some("tool_calls"),
            Some(reason) if !reason.is_empty() => Some(reason),
            _ if !self.tool_calls.is_empty() => Some("tool_calls"),
            _ => None,
        };

        let content = if self.content.is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(&self.content))
        };

        let reasoning_content = self.reasoning.as_ref().map(|r| String::from_utf8_lossy(r));

        let tool_calls = if self.tool_calls.is_empty() {
            None
        } else {
            Some(
                self.tool_calls
                    .iter()
                    .map(|tc| FinishToolCall {
                        id: &tc.id,
                        tool_type: "function",
                        function: FinishFunction {
                            name: &tc.name,
                            arguments: &tc.arguments,
                        },
                    })
                    .collect(),
            )
        };

        let raw_response_body = if (self.partial
            || (self.content.is_empty() && self.tool_calls.is_empty()))
            && !self.raw_response_body.is_empty()
        {
            Some(String::from_utf8_lossy(&self.raw_response_body))
        } else {
            None
        };

        let response = FinishResponse {
            id: chunk_id,
            object: "chat.completion",
            created,
            model,
            choices: [FinishChoice {
                index: 0,
                message: FinishMessage {
                    role: "assistant",
                    content,
                    reasoning_content,
                    tool_calls,
                    truncated: self.truncated,
                    partial: self.partial,
                },
                finish_reason,
            }],
            usage: self.usage.as_ref(),
            raw_response_body,
        };

        serde_json::to_value(response).unwrap_or_default()
    }
}

#[derive(Serialize)]
struct FinishResponse<'a> {
    id: &'a str,
    object: &'static str,
    created: u64,
    model: &'a str,
    choices: [FinishChoice<'a>; 1],
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<&'a OpenAIUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_response_body: Option<std::borrow::Cow<'a, str>>,
}

#[derive(Serialize)]
struct FinishChoice<'a> {
    index: u64,
    message: FinishMessage<'a>,
    finish_reason: Option<&'a str>,
}

#[derive(Serialize)]
struct FinishMessage<'a> {
    role: &'static str,
    content: Option<std::borrow::Cow<'a, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<std::borrow::Cow<'a, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<FinishToolCall<'a>>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    truncated: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    partial: bool,
}

#[derive(Serialize)]
struct FinishToolCall<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    tool_type: &'static str,
    function: FinishFunction<'a>,
}

#[derive(Serialize)]
struct FinishFunction<'a> {
    name: &'a str,
    arguments: &'a str,
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
