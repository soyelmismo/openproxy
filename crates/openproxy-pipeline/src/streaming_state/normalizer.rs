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

    /// Process a tool_call delta. Returns the `arguments` value that
    /// should be sent to the client (just the new fragment, not the
    /// running total). If the upstream already sends fragments (the
    /// correct behavior), this is a no-op — the fragment is returned
    /// as-is and the running total is updated.
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
    let Some(content) = delta.content.as_ref() else {
        return false;
    };
    let mut modified = false;
    let (clean_content, extracted_reasoning) = think_extractor.process(content);
    if clean_content != *content {
        delta.content = Some(std::borrow::Cow::Owned(clean_content));
        modified = true;
    }

    let has_native_reasoning =
        delta.reasoning_content.is_some() || delta.extra.contains_key("reasoning_content");
    if !extracted_reasoning.is_empty() && !has_native_reasoning {
        delta.reasoning_content = Some(std::borrow::Cow::Owned(extracted_reasoning));
        modified = true;
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

/// Modular streaming stage that normalizes non-standard reasoning fields,
/// extracts `<think>` blocks into `reasoning_content`, extracts inline tool calls,
/// and normalizes tool call arguments.
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
