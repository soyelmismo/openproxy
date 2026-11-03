use crate::streaming::{StreamAction, StreamingChunkStage};
use crate::think_extractor::ThinkStreamExtractor;

/// Maximum allowed length for accumulated tool call arguments string.
/// Prevents unbounded memory growth from malicious or buggy upstream.
const MAX_TOOL_CALL_ARGS_BYTES: usize = 1_048_576; // 1 MiB
const MAX_TOOL_CALL_INDICES: usize = 256;

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

    /// Returns the `arguments` value to forward: the new fragment, never
    /// the running total. An upstream that already streams fragments is
    /// forwarded as-is with the running total updated alongside.
    pub fn process<'a>(&mut self, index: u64, arguments: &'a str) -> &'a str {
        if !self.args_by_index.contains_key(&index)
            && self.args_by_index.len() >= MAX_TOOL_CALL_INDICES
        {
            return "";
        }
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
    let Some(content_cow) = delta.content.take() else {
        return false;
    };
    let mut modified = false;

    // Check if the upstream nested an entire SSE frame into delta.content
    let (target_content, direct_reasoning) =
        if let Some((unwrapped_content, unwrapped_reasoning, is_done)) =
            unwrap_nested_sse_content(&content_cow)
        {
            modified = true;
            if is_done {
                delta.content = Some(std::borrow::Cow::Borrowed(""));
                return true;
            }
            (
                unwrapped_content.map(|c| c.into_owned()),
                unwrapped_reasoning.map(|r| r.into_owned()),
            )
        } else {
            (Some(content_cow.to_string()), None)
        };

    if let Some(r) = direct_reasoning {
        let has_native_reasoning =
            delta.reasoning_content.is_some() || delta.extra.contains_key("reasoning_content");
        if !has_native_reasoning {
            delta.reasoning_content = Some(std::borrow::Cow::Owned(r));
            modified = true;
        }
    }

    if let Some(c) = target_content {
        let (clean_content, extracted_reasoning) = think_extractor.process(&c);
        let has_native_reasoning =
            delta.reasoning_content.is_some() || delta.extra.contains_key("reasoning_content");
        if !extracted_reasoning.is_empty() && !has_native_reasoning {
            delta.reasoning_content = Some(std::borrow::Cow::Owned(extracted_reasoning));
            modified = true;
        }
        if clean_content != *content_cow {
            modified = true;
        }
        delta.content = Some(std::borrow::Cow::Owned(clean_content));
    } else {
        if !content_cow.is_empty() {
            modified = true;
        }
        delta.content = Some(std::borrow::Cow::Borrowed(""));
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
pub(crate) struct FastChunk<'a> {
    #[serde(borrow, skip_serializing_if = "Option::is_none")]
    choices: Option<Vec<FastChoice<'a>>>,
    #[serde(flatten)]
    extra: std::collections::HashMap<String, serde_json::Value>,
}
#[derive(serde::Deserialize, serde::Serialize)]
pub(crate) struct FastChoice<'a> {
    #[serde(borrow, skip_serializing_if = "Option::is_none")]
    delta: Option<FastDelta<'a>>,
    #[serde(flatten)]
    extra: std::collections::HashMap<String, serde_json::Value>,
}
#[derive(serde::Deserialize, serde::Serialize)]
struct FastDelta<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<std::borrow::Cow<'a, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow, skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<FastToolCall<'a>>>,
    #[serde(flatten)]
    extra: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(serde::Deserialize)]
struct NestedOpenAiChunkProbe<'a> {
    #[serde(borrow)]
    choices: Option<Vec<NestedOpenAiChoiceProbe<'a>>>,
    #[serde(borrow)]
    response: Option<std::borrow::Cow<'a, str>>,
}

#[derive(serde::Deserialize)]
struct NestedOpenAiChoiceProbe<'a> {
    #[serde(borrow)]
    delta: Option<NestedOpenAiDeltaProbe<'a>>,
}

#[derive(serde::Deserialize)]
struct NestedOpenAiDeltaProbe<'a> {
    #[serde(borrow)]
    content: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    reasoning_content: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    reasoning: Option<std::borrow::Cow<'a, str>>,
}

type NestedSseUnwrapResult<'a> = (
    Option<std::borrow::Cow<'a, str>>,
    Option<std::borrow::Cow<'a, str>>,
    bool,
);

/// Unwraps double-wrapped SSE chunks where an upstream (e.g. misconfigured
/// Cloudflare Worker) literally places an entire inner SSE data line or Cloudflare
/// AI response chunk into `delta.content`.
///
/// Returns `Some((clean_content, extracted_reasoning, is_done))` when nested SSE was detected.
fn unwrap_nested_sse_content<'a>(content: &'a str) -> Option<NestedSseUnwrapResult<'a>> {
    let trimmed = content.trim();
    if !trimmed.starts_with("data:") {
        return None;
    }
    let payload = trimmed[5..].trim();
    if payload == "[DONE]" {
        return Some((None, None, true));
    }
    if !payload.starts_with('{') {
        return None;
    }

    let probe = serde_json::from_str::<NestedOpenAiChunkProbe<'a>>(payload).ok()?;
    if let Some(choices) = probe.choices {
        if let Some(choice) = choices.into_iter().next()
            && let Some(delta) = choice.delta
        {
            let reasoning = delta.reasoning_content.or(delta.reasoning);
            return Some((delta.content, reasoning, false));
        }
        // If choices is empty (e.g. usage-only or terminal metadata chunk), treat as empty content
        return Some((Some(std::borrow::Cow::Borrowed("")), None, false));
    }

    if let Some(resp) = probe.response {
        return Some((Some(resp), None, false));
    }

    // Any other valid JSON inside `data: {...}` that has usage or object metadata
    if payload.contains("\"usage\"") || payload.contains("\"object\"") {
        return Some((Some(std::borrow::Cow::Borrowed("")), None, false));
    }

    None
}
#[derive(serde::Deserialize, serde::Serialize)]
struct FastToolCall<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    index: Option<u64>,
    #[serde(borrow, skip_serializing_if = "Option::is_none")]
    function: Option<FastFunction<'a>>,
    #[serde(flatten)]
    extra: std::collections::HashMap<String, serde_json::Value>,
}
#[derive(serde::Deserialize, serde::Serialize)]
struct FastFunction<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<std::borrow::Cow<'a, str>>,
    #[serde(flatten)]
    extra: std::collections::HashMap<String, serde_json::Value>,
}

pub(crate) fn apply_reasoning_normalizations(
    payload: &str,
    think_extractor: &mut crate::think_extractor::ThinkStreamExtractor,
    tool_call_acc: &mut ToolCallAccumulator,
) -> Option<String> {
    let normalized = crate::sse_accumulator::normalize_nonstandard_reasoning_fields(payload);
    let p: &str = normalized.as_deref().unwrap_or(payload);

    // Role-only and finish chunks carry neither key, so the JSON parse
    // is skipped entirely.
    // Escapes may hide property names or think tags from raw-byte probes.
    // Fall back to serde rather than interpreting JSON with substring tests.
    let escaped = p.contains('\\');
    let has_content = escaped || p.contains("\"content\"");
    let has_tool_calls = escaped || p.contains("\"tool_calls\"");
    if !has_content && !has_tool_calls {
        return normalized;
    }

    // Every tool delta must initialize/update its accumulator, including the
    // first one. Only unescaped ordinary text that has no think tags and no nested SSE
    // can bypass deserialization.
    let has_nested_sse = p.contains("data:");
    if !has_tool_calls
        && (!has_content
            || (!escaped && !has_nested_sse && !think_extractor.has_active_state_or_tag(p)))
    {
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

/// Normalizes non-standard reasoning fields, lifts `<think>` blocks into
/// `reasoning_content`, extracts inline tool calls and normalizes tool call
/// arguments.
#[derive(Default)]
pub struct ReasoningNormalizer {
    pub think_extractor: ThinkStreamExtractor,
    pub tool_call_acc: ToolCallAccumulator,
    pub inline_tool_extractor: crate::inline_tools::InlineToolStreamExtractor,
}

impl ReasoningNormalizer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn finalize(&mut self) -> Option<String> {
        <Self as StreamingChunkStage>::finalize(self)
    }
}

impl StreamingChunkStage for ReasoningNormalizer {
    fn process_chunk(&mut self, payload: &str) -> StreamAction {
        let payload_after_reasoning = match apply_reasoning_normalizations(
            payload,
            &mut self.think_extractor,
            &mut self.tool_call_acc,
        ) {
            Some(modified) => std::borrow::Cow::Owned(modified),
            None => std::borrow::Cow::Borrowed(payload),
        };

        match self
            .inline_tool_extractor
            .process_chunk(&payload_after_reasoning)
        {
            StreamAction::Skip => StreamAction::Skip,
            StreamAction::Mutate(s) => StreamAction::Mutate(s),
            StreamAction::Done => StreamAction::Done,
            StreamAction::Passthrough => match payload_after_reasoning {
                std::borrow::Cow::Owned(m) => StreamAction::Mutate(m),
                std::borrow::Cow::Borrowed(_) => StreamAction::Passthrough,
            },
        }
    }

    fn finalize(&mut self) -> Option<String> {
        if let Some(tool_chunk) = self.inline_tool_extractor.finalize() {
            return Some(tool_chunk);
        }
        let (clean_content, _) = self.think_extractor.flush();
        if clean_content.is_empty() {
            None
        } else {
            Some(clean_content)
        }
    }
}
