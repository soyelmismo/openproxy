//! Extract `<think>...</think>` blocks from the `content` field and
//! move them to `reasoning_content`.
//!
//! ## The problem
//!
//! Some LLM providers (DeepSeek, Qwen, Gemini via certain frontends,
//! open-source models served via vLLM/Ollama) send the model's
//! chain-of-thought reasoning **interleaved with the final answer**
//! inside the `content` field, wrapped in `<think>...</think>` tags:
//!
//! ```json
//! {"choices":[{"delta":{"content":"<think>\nLet me think...\n</think>\nThe answer is 42."}}]}
//! ```
//!
//! Clients that parse `<think>` tags (Cursor, Cline, OpenCode) extract
//! the reasoning into a separate panel — but if the proxy ALSO
//! forwards the raw `content`, the reasoning appears **twice**: once
//! in the reasoning panel and once in the visible response. Clients
//! that DON'T parse `<think>` tags show the raw tags to the user,
//! which is ugly.
//!
//! ## The solution
//!
//! This module provides two functions:
//!
//! 1. [`extract_think_from_content`] — for non-streaming responses.
//!    Takes the full `content` string, extracts all `<think>` blocks,
//!    and returns `(clean_content, reasoning_content)`.
//!
//! 2. [`ThinkStreamExtractor`] — for streaming responses. A stateful
//!    parser that processes `content` deltas chunk-by-chunk and emits
//!    `(content_delta, reasoning_delta)` pairs. The `<think>` tags may
//!    span multiple chunks, so the extractor maintains a state machine
//!    to track whether we're currently inside a think block.
//!
//! ## Supported tag formats
//!
//! - `<think>...</think>` (DeepSeek, Qwen)
//! - `<thinking>...</thinking>` (Anthropic-style, some wrappers)
//! - `<reasoning>...</reasoning>` (some providers)
//!
//! The extractor is case-insensitive for the tag name and handles
//! whitespace after the opening tag.

/// Tags that are recognized as reasoning blocks.
const THINK_OPEN_TAGS: &[&str] = &["<think>", "<thinking>", "<reasoning>", "<thought>"];
const THINK_CLOSE_TAGS: &[&str] = &["</think>", "</thinking>", "</reasoning>", "</thought>"];
const THINK_OPEN_TAGS_BYTES: &[&[u8]] = &[b"<think>", b"<thinking>", b"<reasoning>", b"<thought>"];

/// Find the first case-insensitive match of a needle in a haystack without allocating.
fn find_ignore_ascii_case(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if haystack.len() < needle.len() {
        return None;
    }
    let haystack_bytes = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();

    haystack_bytes
        .windows(needle_bytes.len())
        .position(|w| w.eq_ignore_ascii_case(needle_bytes))
}

#[inline]
fn safe_slice_to(s: &str, end: usize) -> &str {
    let mut e = std::cmp::min(end, s.len());
    while e > 0 && !s.is_char_boundary(e) {
        e -= 1;
    }
    &s[..e]
}

#[inline]
fn safe_slice_from(s: &str, start: usize) -> &str {
    let mut s_idx = std::cmp::min(start, s.len());
    while s_idx < s.len() && !s.is_char_boundary(s_idx) {
        s_idx += 1;
    }
    &s[s_idx..]
}

/// Extract `<think>` blocks from a non-streaming `OpenAIResponse`'s
/// message content and move them to `reasoning_content`.
///
/// For each choice's assistant message:
/// 1. If `content` is a string, extract `<think>` blocks from it.
/// 2. If think blocks are found, set `content` to the cleaned text
///    (without the `<think>` tags).
/// 3. If `reasoning_content` does NOT already exist (the upstream did
///    not send it natively), set `reasoning_content` to the extracted
///    think text. If `reasoning_content` already exists, the upstream
///    is already providing reasoning natively — we DON'T merge in the
///    extracted text (it would be a duplicate of what the upstream
///    sent, since some providers emit the same reasoning in BOTH a
///    `reasoning_content` field AND `<think>` tags inside `content`).
fn apply_extracted_reasoning(choice: &mut crate::translation::Choice, reasoning: String) {
    let existing_rc = choice
        .message
        .extra
        .get("reasoning_content")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if existing_rc.is_empty() {
        choice.message.extra.insert(
            "reasoning_content".to_string(),
            serde_json::Value::String(reasoning),
        );
    }
}

fn process_choice_think(choice: &mut crate::translation::Choice) {
    if choice.message.role != "assistant" {
        return;
    }
    let Some(serde_json::Value::String(content_str)) = &choice.message.content else {
        return;
    };
    let extracted = extract_think_from_content(content_str.as_str());
    let has_reasoning = extracted.has_reasoning();
    let content_changed = extracted.content != *content_str;
    if !has_reasoning && !content_changed {
        return;
    }
    if content_changed {
        choice.message.content = Some(serde_json::Value::String(extracted.content));
    }
    if has_reasoning {
        apply_extracted_reasoning(choice, extracted.reasoning);
    }
}

pub fn extract_think_from_response(
    mut resp: crate::translation::OpenAIResponse,
) -> crate::translation::OpenAIResponse {
    for choice in &mut resp.choices {
        process_choice_think(choice);
    }
    resp
}

/// Result of extracting think blocks from a non-streaming response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtractedThink {
    /// The content with all `<think>` blocks removed. May be empty
    /// if the entire response was reasoning.
    pub content: String,
    /// The concatenated reasoning from all `<think>` blocks.
    /// Empty if no think blocks were found.
    pub reasoning: String,
}

impl ExtractedThink {
    /// True if any think blocks were found.
    pub fn has_reasoning(&self) -> bool {
        !self.reasoning.is_empty()
    }
}

fn find_matching_close_tag(after_open: &str) -> Option<&'static str> {
    let after_open_bytes = after_open.as_bytes();
    for (i, open_tag_bytes) in THINK_OPEN_TAGS_BYTES.iter().enumerate() {
        if after_open_bytes.len() >= open_tag_bytes.len()
            && after_open_bytes[..open_tag_bytes.len()].eq_ignore_ascii_case(open_tag_bytes)
        {
            return Some(THINK_CLOSE_TAGS[i]);
        }
    }
    None
}

fn parse_think_segment(remaining: &str, tag_idx: usize) -> (&str, &str, &str) {
    let content_before = safe_slice_to(remaining, tag_idx);
    let after_open = safe_slice_from(remaining, tag_idx);
    let close_tag = find_matching_close_tag(after_open);
    let after_tag_content = safe_slice_from(
        after_open,
        after_open.find('>').map_or(after_open.len(), |p| p + 1),
    );

    let (think_text, rest) = match close_tag {
        Some(ct) => match find_ignore_ascii_case(after_tag_content, ct) {
            Some(pos) => (
                safe_slice_to(after_tag_content, pos),
                safe_slice_from(after_tag_content, pos + ct.len()),
            ),
            None => (after_tag_content, ""),
        },
        None => (after_tag_content, ""),
    };
    (content_before, think_text, rest)
}

fn append_think_segment_reasoning(reasoning: &mut String, think_text: &str) {
    let trimmed = think_text.trim();
    if !trimmed.is_empty() {
        if !reasoning.is_empty() {
            reasoning.push('\n');
        }
        reasoning.push_str(trimmed);
    }
}

/// Extract all `<think>...</think>` blocks from a content string.
///
/// Handles interleaved reasoning: `<think>A</think>B<think>C</think>D`
/// produces `content = "BD"` and `reasoning = "AC"`.
///
/// The tags are matched case-insensitively. Whitespace between the
/// opening tag and the content is trimmed from the reasoning. If the
/// closing tag is missing, everything after the opening tag is treated
/// as reasoning (the model didn't finish the think block properly).
pub fn extract_think_from_content(content: &str) -> ExtractedThink {
    let mut result = ExtractedThink::default();
    let mut remaining = content;

    while let Some((tag_idx, _tag_name)) = find_earliest_tag(remaining, THINK_OPEN_TAGS) {
        let (content_before, think_text, rest) = parse_think_segment(remaining, tag_idx);
        result.content.push_str(content_before);
        append_think_segment_reasoning(&mut result.reasoning, think_text);
        remaining = rest;
    }
    result.content.push_str(remaining);

    // Strip orphaned close tags (</think>, </thinking>, etc.) that
    // appear in the content without a matching open tag. Some
    // providers emit duplicate or stray close tags like:
    //   <think>reasoning</think>\n\n</think>
    result.content = strip_orphaned_close_tags(&result.content);

    // Trim leading whitespace from the final content that was between
    // the closing </think> tag and the start of the actual answer.
    result.content = result.content.trim_start_matches('\n').to_string();

    result
}

/// Find the earliest occurrence of any of the given tags in `s`.
/// Returns `(byte_offset, tag_string)`.
fn find_earliest_tag<'a>(s: &str, tags: &[&'a str]) -> Option<(usize, &'a str)> {
    tags.iter()
        .filter_map(|tag| find_ignore_ascii_case(s, tag).map(|pos| (pos, *tag)))
        .min_by_key(|(pos, _)| *pos)
}

/// Remove orphaned close tags (</think>, </thinking>, etc.) from a
/// string. An orphaned close tag is one that appears without a
/// matching open tag before it. Some providers emit duplicate or
/// stray close tags like:
///   <think>reasoning</think>\n\n</think>
/// After the first </think> is matched by the extractor, the second
/// </think> remains as orphaned content. This function removes it.
fn strip_orphaned_close_tags(content: &str) -> String {
    if !content.contains('<') {
        return content.to_string();
    }
    let mut result = content.to_string();
    for (i, close_tag) in THINK_CLOSE_TAGS.iter().enumerate() {
        while let Some(pos) = find_ignore_ascii_case(&result, close_tag) {
            // Check that there's no matching open tag before this
            // close tag in the content.
            let open_tag = THINK_OPEN_TAGS[i];
            if find_ignore_ascii_case(safe_slice_to(&result, pos), open_tag).is_some() {
                break;
            }
            // Remove the orphaned close tag in-place.
            result.replace_range(pos..pos + close_tag.len(), "");
        }
    }
    result
}

/// Stateful extractor for streaming responses.
///
/// Processes `content` deltas one at a time and emits
/// `(content_delta, reasoning_delta)` pairs. The `<think>` tags may
/// span multiple chunks, so the extractor maintains a buffer to handle
/// partial tags.
///
/// # Usage
///
/// ```ignore
/// let mut extractor = ThinkStreamExtractor::new();
/// for delta in streaming_deltas {
///     let (content, reasoning) = extractor.process(&delta);
///     if !reasoning.is_empty() {
///         // emit a chunk with reasoning_content
///     }
///     if !content.is_empty() {
///         // emit a chunk with content
///     }
/// }
/// // After the stream ends, flush any remaining buffer:
/// let (content, reasoning) = extractor.flush();
/// ```
#[derive(Debug, Clone)]
pub struct ThinkStreamExtractor {
    /// Current state: are we inside a `<think>` block?
    inside_think: bool,
    /// Buffer for content that might be part of a tag that spans
    /// chunk boundaries. E.g. if we receive "<thin" we buffer it
    /// until we can determine if it's "<think>" or just text.
    tag_buffer: String,
    /// Which close tag we're looking for (set when we enter a think block).
    close_tag: Option<String>,
}

impl ThinkStreamExtractor {
    pub fn new() -> Self {
        Self {
            inside_think: false,
            tag_buffer: String::new(),
            close_tag: None,
        }
    }

    fn process_fast_path(&self, delta: &str) -> (String, String) {
        if self.inside_think {
            (String::new(), delta.to_string())
        } else {
            (delta.to_string(), String::new())
        }
    }

    /// Process a content delta. Returns `(content_delta, reasoning_delta)`.
    ///
    /// The returned content_delta has `<think>` blocks removed. The
    /// reasoning_delta contains text from inside `<think>` blocks.
    /// Both may be empty.
    pub fn process(&mut self, delta: &str) -> (String, String) {
        if delta.is_empty() {
            return (String::new(), String::new());
        }

        if self.tag_buffer.is_empty() && !delta.contains('<') {
            return self.process_fast_path(delta);
        }

        let mut input = std::mem::take(&mut self.tag_buffer);
        input.push_str(delta);

        if self.inside_think {
            self.process_inside_think(&input)
        } else {
            self.process_outside_think(&input)
        }
    }

    /// Flush any remaining buffer. Call this when the stream ends.
    pub fn flush(&mut self) -> (String, String) {
        let buffered = std::mem::take(&mut self.tag_buffer);
        if buffered.is_empty() {
            return (String::new(), String::new());
        }
        if self.inside_think {
            // Unterminated think block — treat remaining as reasoning.
            (String::new(), buffered)
        } else {
            // Buffered text that wasn't a tag — emit as content.
            (buffered, String::new())
        }
    }
}

impl crate::streaming::StreamingChunkStage for ThinkStreamExtractor {
    fn process_chunk(&mut self, payload: &str) -> crate::streaming::StreamAction {
        let (clean, reasoning) = self.process(payload);
        if clean != payload || !reasoning.is_empty() {
            crate::streaming::StreamAction::Mutate(clean)
        } else {
            crate::streaming::StreamAction::Passthrough
        }
    }

    fn finalize(&mut self) -> Option<String> {
        let (clean, _) = self.flush();
        if clean.is_empty() { None } else { Some(clean) }
    }
}

impl ThinkStreamExtractor {
    fn process_outside_think(&mut self, input: &str) -> (String, String) {
        let Some((tag_pos, tag_str)) = find_earliest_tag(input, THINK_OPEN_TAGS) else {
            // No opening tag found. But the end of the input might
            // be the start of a tag (e.g. "<thi"). Check if the
            // input ends with a partial tag prefix and buffer it.
            let safe_len = find_safe_split_point(input);
            let content = safe_slice_to(input, safe_len).to_string();
            self.tag_buffer = safe_slice_from(input, safe_len).to_string();
            // Strip orphaned close tags from the content (e.g.
            // stray </think> without a matching <think>).
            let cleaned = strip_orphaned_close_tags(&content);
            return (cleaned, String::new());
        };

        // Found an opening tag. Emit content before it.
        let mut content_before = safe_slice_to(input, tag_pos).to_string();
        let after_tag = safe_slice_from(input, tag_pos);

        // Determine the close tag we're looking for.
        let close_tag = THINK_OPEN_TAGS
            .iter()
            .position(|&ot| tag_str.eq_ignore_ascii_case(ot))
            .map(|i| THINK_CLOSE_TAGS[i].to_string());

        self.close_tag = close_tag;
        self.inside_think = true;

        // Skip past the opening tag.
        let after_tag_content = after_tag.get(tag_str.len()..).unwrap_or("");

        if after_tag_content.is_empty() {
            // The tag was exactly at the end — nothing more to process.
            return (content_before, String::new());
        }

        // Process the remaining content as inside-think.
        let (more_content, reasoning) = self.process_inside_think(after_tag_content);
        // content_before is the text before <think>, more_content should
        // be empty (we're inside think now) but just in case.
        if !more_content.is_empty() {
            content_before.push_str(&more_content);
        }
        (content_before, reasoning)
    }

    fn process_inside_think(&mut self, input: &str) -> (String, String) {
        let close_tag = match &self.close_tag {
            Some(ct) => ct.clone(),
            None => {
                // Shouldn't happen, but handle gracefully.
                self.inside_think = false;
                return (input.to_string(), String::new());
            }
        };

        match find_ignore_ascii_case(input, &close_tag) {
            Some(pos) => {
                // Found closing tag. Everything before it is reasoning.
                let mut reasoning = safe_slice_to(input, pos).to_string();
                let after_close = safe_slice_from(input, pos + close_tag.len());
                self.inside_think = false;
                self.close_tag = None;

                if after_close.is_empty() {
                    return (String::new(), reasoning);
                }

                // Process remaining content as outside-think.
                let (more_content, more_reasoning) = self.process_outside_think(after_close);
                if !more_reasoning.is_empty() {
                    reasoning.push_str(&more_reasoning);
                }
                (more_content, reasoning)
            }
            None => {
                // No closing tag found. But the end of the input might
                // be the start of the close tag. Buffer the potential
                // partial close tag.
                let safe_len = find_safe_split_point_close(input, &close_tag);
                let reasoning = safe_slice_to(input, safe_len).to_string();
                self.tag_buffer = safe_slice_from(input, safe_len).to_string();
                (String::new(), reasoning)
            }
        }
    }
}

impl Default for ThinkStreamExtractor {
    fn default() -> Self {
        Self::new()
    }
}

fn is_partial_open_tag_tail(tail: &str) -> bool {
    let tail_bytes = tail.as_bytes();
    THINK_OPEN_TAGS.iter().any(|tag| {
        tag.as_bytes().starts_with(tail_bytes)
            || (tag.len() >= tail.len()
                && tag.as_bytes()[..tail.len()].eq_ignore_ascii_case(tail_bytes))
    })
}

/// Find the latest position in `input` where we can safely split
/// without cutting a potential opening tag. Everything after this
/// position might be the start of a `<think>` tag.
fn find_safe_split_point(input: &str) -> usize {
    if !input.contains('<') {
        return input.len();
    }

    let max_tag_len = THINK_OPEN_TAGS.iter().map(|t| t.len()).max().unwrap_or(0);
    let check_len = std::cmp::min(max_tag_len.saturating_sub(1), input.len());
    (1..=check_len)
        .rev()
        .map(|partial_len| input.len() - partial_len)
        .find(|&split_byte| {
            input.is_char_boundary(split_byte) && is_partial_open_tag_tail(&input[split_byte..])
        })
        .unwrap_or(input.len())
}

/// Find the latest position in `input` where we can safely split
/// without cutting the `close_tag`. Everything after this position
/// might be the start of the close tag.
fn find_safe_split_point_close(input: &str, close_tag: &str) -> usize {
    if !input.contains('<') {
        return input.len();
    }

    let check_len = std::cmp::min(close_tag.len() - 1, input.len());
    for partial_len in (1..=check_len).rev() {
        let split_byte = input.len() - partial_len;
        // CRITICAL: same UTF-8 char boundary check
        if !input.is_char_boundary(split_byte) {
            continue;
        }
        let tail = &input[split_byte..];
        let tail_bytes = tail.as_bytes();

        if close_tag.as_bytes()[..tail.len()].eq_ignore_ascii_case(tail_bytes) {
            return split_byte;
        }
    }
    input.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Non-streaming: extract_think_from_content
    // -----------------------------------------------------------------

    #[test]
    fn no_think_tags() {
        let r = extract_think_from_content("Hello, world!");
        assert_eq!(r.content, "Hello, world!");
        assert_eq!(r.reasoning, "");
        assert!(!r.has_reasoning());
    }

    #[test]
    fn simple_think_block() {
        let r = extract_think_from_content("<think>\nLet me think...\n</think>\nThe answer is 42.");
        assert_eq!(r.content, "The answer is 42.");
        assert_eq!(r.reasoning, "Let me think...");
        assert!(r.has_reasoning());
    }

    #[test]
    fn interleaved_think_blocks() {
        let r = extract_think_from_content("<think>A</think>B<think>C</think>D");
        assert_eq!(r.content, "BD");
        assert_eq!(r.reasoning, "A\nC");
    }

    #[test]
    fn case_insensitive_tags() {
        let r = extract_think_from_content("<THINK>reasoning</THINK>answer");
        assert_eq!(r.content, "answer");
        assert_eq!(r.reasoning, "reasoning");
    }

    #[test]
    fn thinking_tag() {
        let r = extract_think_from_content("<thinking>my thoughts</thinking>response");
        assert_eq!(r.content, "response");
        assert_eq!(r.reasoning, "my thoughts");
    }

    #[test]
    fn reasoning_tag() {
        let r = extract_think_from_content("<reasoning>logic</reasoning>output");
        assert_eq!(r.content, "output");
        assert_eq!(r.reasoning, "logic");
    }

    #[test]
    fn unterminated_think_block() {
        let r = extract_think_from_content("<think>incomplete reasoning");
        assert_eq!(r.content, "");
        assert_eq!(r.reasoning, "incomplete reasoning");
    }

    #[test]
    fn empty_think_block() {
        let r = extract_think_from_content("<think></think>answer");
        assert_eq!(r.content, "answer");
        assert_eq!(r.reasoning, "");
    }

    #[test]
    fn only_think_block() {
        let r = extract_think_from_content("<think>all reasoning, no answer</think>");
        assert_eq!(r.content, "");
        assert_eq!(r.reasoning, "all reasoning, no answer");
    }

    #[test]
    fn orphaned_close_tag() {
        // Bug: some providers emit a stray </think> after the first
        // close tag:
        //   <think>reasoning</think>\n\n</think>
        // The first </think> is matched, the second is orphaned.
        let r = extract_think_from_content(
            "<think>\nNow I have a comprehensive understanding.\n</think>\n\n</think>",
        );
        assert_eq!(r.content, "");
        assert_eq!(r.reasoning, "Now I have a comprehensive understanding.");
    }

    #[test]
    fn orphaned_close_tag_with_content_after() {
        // Same bug but with actual content after the orphaned tag.
        let r = extract_think_from_content("<think>reasoning</think>\n\n</think>\nThe answer.");
        assert_eq!(r.content, "The answer.");
        assert_eq!(r.reasoning, "reasoning");
    }

    // -----------------------------------------------------------------
    // Streaming: ThinkStreamExtractor
    // -----------------------------------------------------------------

    #[test]
    fn stream_simple() {
        let mut ext = ThinkStreamExtractor::new();
        let (c1, r1) = ext.process("<think>");
        assert_eq!(c1, "");
        assert_eq!(r1, "");
        let (c2, r2) = ext.process("reasoning here");
        assert_eq!(c2, "");
        assert_eq!(r2, "reasoning here");
        let (c3, r3) = ext.process("</think>");
        assert_eq!(c3, "");
        assert_eq!(r3, "");
        let (c4, r4) = ext.process("final answer");
        assert_eq!(c4, "final answer");
        assert_eq!(r4, "");
        let (cf, rf) = ext.flush();
        assert_eq!(cf, "");
        assert_eq!(rf, "");
    }

    #[test]
    fn stream_tag_split_across_chunks() {
        let mut ext = ThinkStreamExtractor::new();
        // "<thi" might be start of "<think>"
        let (c1, _) = ext.process("Hello <thi");
        assert_eq!(c1, "Hello ");
        let (c2, r2) = ext.process("nk>reasoning</think> world");
        assert_eq!(c2, " world");
        assert_eq!(r2, "reasoning");
    }

    #[test]
    fn stream_close_tag_split() {
        let mut ext = ThinkStreamExtractor::new();
        ext.process("<think>");
        let (c1, r1) = ext.process("some reasoning here</thin");
        assert_eq!(c1, "");
        assert_eq!(r1, "some reasoning here");
        let (c2, r2) = ext.process("k>answer");
        assert_eq!(c2, "answer");
        assert_eq!(r2, "");
    }

    #[test]
    fn stream_no_tags() {
        let mut ext = ThinkStreamExtractor::new();
        let (c1, r1) = ext.process("just a normal ");
        let (c2, r2) = ext.process("response");
        assert_eq!(c1, "just a normal ");
        assert_eq!(r1, "");
        assert_eq!(c2, "response");
        assert_eq!(r2, "");
    }

    #[test]
    fn stream_interleaved() {
        let mut ext = ThinkStreamExtractor::new();
        let (c, r) = ext.process("<think>A</think>B<think>C</think>D");
        assert_eq!(c, "BD");
        assert_eq!(r, "AC");
    }

    #[test]
    fn stream_flush_unterminated() {
        let mut ext = ThinkStreamExtractor::new();
        let (_, _) = ext.process("<think>");
        let (c, r) = ext.process("incomplete");
        // "incomplete" was already emitted as reasoning during process().
        assert_eq!(c, "");
        assert_eq!(r, "incomplete");
        // Flush: nothing left in the buffer.
        let (cf, rf) = ext.flush();
        assert_eq!(cf, "");
        assert_eq!(rf, "");
    }

    #[test]
    fn stream_flush_partial_tag() {
        let mut ext = ThinkStreamExtractor::new();
        ext.process("hello <thi");
        let (c, r) = ext.flush();
        assert_eq!(c, "<thi");
        assert_eq!(r, "");
    }

    // -----------------------------------------------------------------
    // Regression: non-streaming response with BOTH `reasoning_content`
    // (sent natively by the upstream) AND `<think>` tags inside
    // `content` (also sent by the upstream, duplicating the same text).
    // The proxy must strip `<think>` from content (so the visible
    // response is clean) but MUST NOT merge the extracted text into
    // `reasoning_content` (it's already there natively; merging would
    // duplicate it). This is the MiniMax-M3-via-tokenrouter bug.
    // -----------------------------------------------------------------
    #[test]
    fn non_streaming_no_duplicate_when_native_reasoning_present() {
        use crate::translation::{OpenAIChoice, OpenAIResponse};
        use openproxy_types::OpenAIMessage;
        let mut resp = OpenAIResponse {
            id: "test".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "MiniMax-M3".to_string(),
            choices: vec![OpenAIChoice {
                index: 0,
                message: OpenAIMessage {
                    role: "assistant".to_string(),
                    content: Some(serde_json::Value::String(
                        "<think>Let me think about this.</think>The answer is 42.".to_string(),
                    )),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    extra: {
                        let mut m = serde_json::Map::new();
                        m.insert(
                            "reasoning_content".to_string(),
                            serde_json::Value::String("Let me think about this.".to_string()),
                        );
                        m
                    },
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: None,
        };
        resp = extract_think_from_response(resp);
        // Content should be cleaned (no <think> tags).
        let content = resp.choices[0]
            .message
            .content
            .as_ref()
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(content, "The answer is 42.");
        // reasoning_content should NOT be duplicated — the upstream's
        // native value should be preserved as-is.
        let rc = resp.choices[0]
            .message
            .extra
            .get("reasoning_content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(
            rc, "Let me think about this.",
            "reasoning_content must not be duplicated when the upstream sent it natively; got: {rc:?}"
        );
    }

    // -----------------------------------------------------------------
    // Same scenario but streaming. Three chunks模拟 MiniMax-M3's
    // behavior: each chunk carries BOTH `delta.reasoning` AND
    // `delta.content` with the same text wrapped in `<think>` (opening
    // on chunk 1, body on chunk 2, closing + answer on chunk 3).
    // After normalization + think extraction, the client should see
    // each chunk's content emptied (no `<think>` leakage) and the
    // upstream's `reasoning_content` preserved (no duplication).
    // -----------------------------------------------------------------
    #[test]
    fn streaming_no_duplicate_when_native_reasoning_present() {
        // This test is at the pipeline level (apply_reasoning_normalizations),
        // but we verify the underlying ThinkStreamExtractor behaves
        // correctly: when content has `<think>` tags, it strips them
        // and returns the extracted text. The pipeline-level dedup
        // (don't add extracted text to reasoning_content when it
        // already exists) is tested via the pipeline tests.
        let mut ext = ThinkStreamExtractor::new();
        // Chunk 1: opening <think> + first part of reasoning.
        let (c1, r1) = ext.process("<think>\nLet me think");
        assert_eq!(c1, "");
        assert_eq!(r1, "\nLet me think");
        // Chunk 2: more reasoning (still inside <think>).
        let (c2, r2) = ext.process(" about this.");
        assert_eq!(c2, "");
        assert_eq!(r2, " about this.");
        // Chunk 3: close </think> + answer.
        let (c3, r3) = ext.process("</think>The answer is 42.");
        assert_eq!(c3, "The answer is 42.");
        assert_eq!(r3, "");
    }

    // -----------------------------------------------------------------
    // Regression: multi-byte UTF-8 content (é, ×, emojis, CJK, etc.)
    // must NOT panic the find_safe_split_point / find_safe_split_point_close
    // functions. Previously, a chunk ending with a multi-byte char
    // whose byte length didn't align with the tag prefix length would
    // cause `&lower[input.len() - partial_len..]` to panic with
    // "byte index N is not a char boundary" — killing the streaming
    // task and leaving the dashboard with a ghost in-flight row that
    // never resolved (no usage row, no terminal stage event).
    // -----------------------------------------------------------------
    #[test]
    fn stream_multibyte_content_does_not_panic() {
        let mut ext = ThinkStreamExtractor::new();
        // Content with multi-byte chars: × (2 bytes), é (2 bytes),
        // and a CJK char (3 bytes). The find_safe_split_point function
        // scans partial tag prefixes from the end of the string; with
        // multi-byte chars at the end, some partial_len values land
        // inside a char and must be skipped, not sliced.
        let (c, _r) = ext.process("35 × 63 = 2205");
        assert!(!c.is_empty(), "content should pass through");
    }

    #[test]
    fn stream_multibyte_inside_think_block_does_not_panic() {
        let mut ext = ThinkStreamExtractor::new();
        ext.process("<think>");
        // Multi-byte chars inside a think block — the
        // find_safe_split_point_close function scans for the close
        // tag prefix and must skip non-char-boundary offsets.
        let (_c, r) = ext.process("calculating 35 × 63");
        assert!(r.contains("×"), "multi-byte char should be in reasoning");
    }

    #[test]
    fn stream_emoji_at_end_does_not_panic() {
        let mut ext = ThinkStreamExtractor::new();
        // Emoji (4 bytes) at the end — the worst case for
        // find_safe_split_point because partial_len values 1, 2, 3
        // all land inside the emoji.
        let (c, _r) = ext.process("The answer is 42 🎉");
        assert!(c.contains("🎉"), "emoji should pass through");
    }

    #[test]
    fn find_safe_split_point_with_multibyte_tail() {
        // Direct test of the function that was panicking.
        // "35 × 63" — the × is at bytes 3..5 (2 bytes).
        // partial_len=1 → byte 6 (ok, '3'), partial_len=2 → byte 5
        // (NOT a char boundary, inside ×), partial_len=3 → byte 4
        // (ok, start of ×), etc. The function must skip byte 5.
        let result = find_safe_split_point("35 × 63");
        // No tag prefix matches, so it should return the full length.
        assert_eq!(result, 8);
    }

    #[test]
    fn find_safe_split_point_close_with_multibyte_tail() {
        // Direct test of find_safe_split_point_close with multi-byte
        // content at the end.
        let result = find_safe_split_point_close("35 × 63", "</think>");
        assert_eq!(result, 8);
    }
}
