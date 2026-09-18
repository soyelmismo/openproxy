//! Types and constants for SSE streaming response accumulation.

/// Maximum number of bytes the accumulator's text fields may collectively
/// hold. After this is reached, additional chunks are dropped and the
/// `truncated` flag is set. Streaming responses are passed chunk-by-chunk to
/// the downstream client without an artificial wire-size ceiling, while this
/// 256 KiB cap exists to bound the per-stream heap footprint of the accumulator
/// itself under high concurrency (e.g. 50 concurrent streams × 256 KiB = 12.8 MiB
/// worst case, down from 200 MiB at 4 MiB).
pub const MAX_ACCUMULATED_BYTES: usize = 256 * 1024; // 262,144 bytes

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

#[derive(serde::Deserialize)]
pub(crate) struct ToolCallProbeOuter<'a> {
    #[serde(borrow)]
    pub choices: Option<Vec<ToolCallProbeChoice<'a>>>,
}

#[derive(serde::Deserialize)]
pub(crate) struct ToolCallProbeChoice<'a> {
    #[serde(borrow)]
    pub delta: Option<ToolCallProbeDelta<'a>>,
}

#[derive(serde::Deserialize)]
pub(crate) struct ToolCallProbeDelta<'a> {
    #[serde(borrow)]
    pub tool_calls: Option<Vec<ToolCallProbe<'a>>>,
}

#[derive(serde::Deserialize)]
pub(crate) struct ToolCallProbe<'a> {
    pub index: Option<usize>,
    #[serde(borrow)]
    pub id: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    pub function: Option<ToolCallFunctionProbe<'a>>,
}

#[derive(serde::Deserialize)]
pub(crate) struct ToolCallFunctionProbe<'a> {
    #[serde(borrow)]
    pub name: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    pub arguments: Option<std::borrow::Cow<'a, str>>,
}
