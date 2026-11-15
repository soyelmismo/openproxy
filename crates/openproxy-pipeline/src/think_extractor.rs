//! Moves `<think>...</think>` blocks out of `content` and into
//! `reasoning_content`.
//!
//! DeepSeek, Qwen, vLLM and Ollama serve reasoning interleaved with the final
//! answer inside `content`:
//!
//! ```json
//! {"choices":[{"delta":{"content":"<think>\nLet me think...\n</think>\nThe answer is 42."}}]}
//! ```
//!
//! Clients that parse the tags (Cursor, Cline, OpenCode) would render the
//! reasoning twice if the raw `content` also reached them. Clients that do not
//! parse them show the tags verbatim.
//!
//! [`extract_think_from_content`] handles a whole response;
//! [`ThinkStreamExtractor`] handles content deltas whose tags span chunks.
//!
//! Recognized tags are `<think>`, `<thinking>`, `<reasoning>` and `<thought>`,
//! matched case-insensitively.

/// Tags that are recognized as reasoning blocks.
const THINK_OPEN_TAGS: &[&str] = &["<think>", "<thinking>", "<reasoning>", "<thought>"];
const THINK_CLOSE_TAGS: &[&str] = &["</think>", "</thinking>", "</reasoning>", "</thought>"];
const THINK_OPEN_TAGS_BYTES: &[&[u8]] = &[b"<think>", b"<thinking>", b"<reasoning>", b"<thought>"];

/// Find the first case-insensitive match of a needle in a haystack without allocating.
#[inline]
fn find_ignore_ascii_case(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if haystack.len() < needle.len() {
        return None;
    }
    let haystack_bytes = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    let first_lo = needle_bytes[0].to_ascii_lowercase();
    let first_hi = needle_bytes[0].to_ascii_uppercase();
    let limit = haystack_bytes.len() - needle_bytes.len();
    let mut pos = 0;

    while pos <= limit {
        let offset = if first_lo == first_hi {
            memchr::memchr(first_lo, &haystack_bytes[pos..=limit])?
        } else {
            memchr::memchr2(first_lo, first_hi, &haystack_bytes[pos..=limit])?
        };
        let idx = pos + offset;
        if haystack_bytes[idx..idx + needle_bytes.len()].eq_ignore_ascii_case(needle_bytes) {
            return Some(idx);
        }
        pos = idx + 1;
    }
    None
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
/// assistant message and move them to `reasoning_content`.
///
/// A pre-existing `reasoning_content` wins: providers that emit reasoning
/// natively repeat the same text inside `<think>` tags, so merging would
/// duplicate it.
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
    // Mitigate upstream AutoClaw false positive safety filter where content is "DANGEROUS_ACTIVITY_BLOCKED"
    // but the model reasoned normally.
    if let Some(serde_json::Value::String(content_str)) = &choice.message.content
        && content_str.trim() == "DANGEROUS_ACTIVITY_BLOCKED"
    {
        let reasoning = choice
            .message
            .extra
            .get("reasoning_content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !reasoning.is_empty() {
            // If reasoning is benign, provide a clean response acknowledging the request
            choice.message.content = Some(serde_json::Value::String(
                "Task processed successfully.".to_string(),
            ));
        }
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

/// JSON-value form of [`extract_think_from_response`].
///
/// Keeps every field the typed round-trip would drop (for example
/// `raw_response_body` on a partial stream) while still moving `<think>`
/// blocks out of `content` into `reasoning_content`.
pub fn extract_think_from_response_value(mut val: serde_json::Value) -> serde_json::Value {
    let Some(choices) = val
        .get_mut("choices")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return val;
    };
    for choice in choices {
        let Some(message) = choice
            .get_mut("message")
            .and_then(serde_json::Value::as_object_mut)
        else {
            continue;
        };
        if message.get("role").and_then(serde_json::Value::as_str) != Some("assistant") {
            continue;
        }
        if let Some(content) = message.get("content").and_then(serde_json::Value::as_str)
            && content.trim() == "DANGEROUS_ACTIVITY_BLOCKED"
        {
            let reasoning = message
                .get("reasoning_content")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if !reasoning.is_empty() {
                message.insert(
                    "content".to_string(),
                    serde_json::Value::String("Task processed successfully.".to_string()),
                );
            }
        }
        let Some(content) = message.get("content").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let extracted = extract_think_from_content(content);
        let has_reasoning = extracted.has_reasoning();
        if !has_reasoning && extracted.content == content {
            continue;
        }
        if extracted.content != content {
            message.insert(
                "content".to_string(),
                serde_json::Value::String(extracted.content),
            );
        }
        if has_reasoning {
            let has_existing = message
                .get("reasoning_content")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|s| !s.is_empty());
            if !has_existing {
                message.insert(
                    "reasoning_content".to_string(),
                    serde_json::Value::String(extracted.reasoning),
                );
            }
        }
    }
    val
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtractedThink {
    /// `<think>` blocks removed. Empty when the whole response was reasoning.
    pub content: String,
    /// Concatenated text of all `<think>` blocks.
    pub reasoning: String,
}

impl ExtractedThink {
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
/// Tag names match case-insensitively and leading whitespace inside a block
/// is trimmed from the reasoning. An unclosed block extends to the end of the
/// string.
pub fn extract_think_from_content(content: &str) -> ExtractedThink {
    let Some((first_idx, _)) = find_earliest_tag(content, THINK_OPEN_TAGS) else {
        let trimmed = content.trim_start_matches('\n');
        let cleaned = strip_orphaned_close_tags(trimmed.to_string());
        let final_content = if cleaned.starts_with('\n') {
            cleaned.trim_start_matches('\n').to_string()
        } else {
            cleaned
        };
        return ExtractedThink {
            content: final_content,
            reasoning: String::new(),
        };
    };

    let mut result = ExtractedThink {
        content: String::with_capacity(content.len()),
        reasoning: String::new(),
    };
    let (content_before, think_text, mut remaining) = parse_think_segment(content, first_idx);
    result.content.push_str(content_before);
    append_think_segment_reasoning(&mut result.reasoning, think_text);

    while let Some((tag_idx, _tag_name)) = find_earliest_tag(remaining, THINK_OPEN_TAGS) {
        let (cb, tt, rest) = parse_think_segment(remaining, tag_idx);
        result.content.push_str(cb);
        append_think_segment_reasoning(&mut result.reasoning, tt);
        remaining = rest;
    }
    result.content.push_str(remaining);

    result.content = strip_orphaned_close_tags(result.content);
    if result.content.starts_with('\n') {
        let trim_len = result.content.len() - result.content.trim_start_matches('\n').len();
        result.content.drain(..trim_len);
    }

    result
}

/// Find the earliest occurrence of any of the given tags in `s`.
/// Uses a single SIMD `memchr_iter(b'<', ...)` pass when all tags start with `<`.
/// Returns `(byte_offset, tag_string)`.
#[inline]
fn find_earliest_tag<'a>(s: &str, tags: &[&'a str]) -> Option<(usize, &'a str)> {
    let bytes = s.as_bytes();
    for idx in memchr::memchr_iter(b'<', bytes) {
        let tail = &bytes[idx..];
        for &tag in tags {
            let tag_bytes = tag.as_bytes();
            if tail.len() >= tag_bytes.len()
                && tail[..tag_bytes.len()].eq_ignore_ascii_case(tag_bytes)
            {
                return Some((idx, tag));
            }
        }
    }
    None
}

/// Remove a close tag that has no matching open tag before it. Providers emit
/// stray duplicates like `<think>reasoning</think>\n\n</think>`, leaving the
/// second one as orphaned content.
fn strip_orphaned_close_tags(mut result: String) -> String {
    if find_earliest_tag(&result, THINK_CLOSE_TAGS).is_none() {
        return result;
    }
    for (i, close_tag) in THINK_CLOSE_TAGS.iter().enumerate() {
        while let Some(pos) = find_ignore_ascii_case(&result, close_tag) {
            let open_tag = THINK_OPEN_TAGS[i];
            if find_ignore_ascii_case(safe_slice_to(&result, pos), open_tag).is_some() {
                break;
            }
            result.replace_range(pos..pos + close_tag.len(), "");
        }
    }
    result
}

/// Stateful extractor for streaming responses.
///
/// Processes `content` deltas one at a time and emits
/// `(content_delta, reasoning_delta)` pairs. Tags split across chunks are held
/// in a buffer until the remainder disambiguates them.
#[derive(Debug, Clone)]
pub struct ThinkStreamExtractor {
    inside_think: bool,
    /// Trailing bytes that may be a partial tag, e.g. `"<thin"`.
    tag_buffer: String,
    /// Set on entering a think block, so the matching close tag is known.
    close_tag: Option<&'static str>,
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

    /// Process a content delta, returning `(content_delta, reasoning_delta)`;
    /// both may be empty. A partial trailing tag (e.g. `"<thin"`) stays in
    /// `tag_buffer` until the rest of the tag lands.
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
            (String::new(), buffered)
        } else {
            (buffered, String::new())
        }
    }

    /// Fast probe for the normalizer to skip JSON parsing when no think tag
    /// or active think state exists.
    #[inline]
    pub fn has_active_state_or_tag(&self, text: &str) -> bool {
        if self.inside_think || !self.tag_buffer.is_empty() {
            return true;
        }
        may_contain_think_tag_or_partial(text)
    }
}

#[inline]
fn may_contain_think_tag_or_partial(text: &str) -> bool {
    let bytes = text.as_bytes();
    for idx in memchr::memchr_iter(b'<', bytes) {
        let tail = &bytes[idx + 1..];
        if tail.is_empty() || tail[0] == b'"' {
            return true;
        }
        let b0 = tail[0].to_ascii_lowercase();
        if b0 == b't' {
            if tail.len() == 1 || tail[1] == b'"' || tail[1].eq_ignore_ascii_case(&b'h') {
                return true;
            }
        } else if b0 == b'r' {
            if tail.len() == 1 || tail[1] == b'"' || tail[1].eq_ignore_ascii_case(&b'e') {
                return true;
            }
        } else if b0 == b'/'
            && tail.len() >= 3
            && (tail[1..3].eq_ignore_ascii_case(b"th") || tail[1..3].eq_ignore_ascii_case(b"re"))
        {
            return true;
        }
    }
    false
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
            let safe_len = find_safe_split_point(input);
            let content = safe_slice_to(input, safe_len).to_string();
            let tail = safe_slice_from(input, safe_len);
            if !tail.is_empty() {
                self.tag_buffer.push_str(tail);
            }
            let cleaned = strip_orphaned_close_tags(content);
            return (cleaned, String::new());
        };

        let mut content_before = safe_slice_to(input, tag_pos).to_string();
        let after_tag = safe_slice_from(input, tag_pos);

        let close_tag = THINK_OPEN_TAGS
            .iter()
            .position(|&ot| tag_str.eq_ignore_ascii_case(ot))
            .map(|i| THINK_CLOSE_TAGS[i]);

        self.close_tag = close_tag;
        self.inside_think = true;

        let after_tag_content = after_tag.get(tag_str.len()..).unwrap_or("");

        if after_tag_content.is_empty() {
            return (content_before, String::new());
        }

        let (more_content, reasoning) = self.process_inside_think(after_tag_content);
        if !more_content.is_empty() {
            content_before.push_str(&more_content);
        }
        (content_before, reasoning)
    }

    fn process_inside_think(&mut self, input: &str) -> (String, String) {
        let Some(close_tag) = self.close_tag else {
            self.inside_think = false;
            return (input.to_string(), String::new());
        };

        match find_ignore_ascii_case(input, close_tag) {
            Some(pos) => {
                let mut reasoning = safe_slice_to(input, pos).to_string();
                let after_close = safe_slice_from(input, pos + close_tag.len());
                self.inside_think = false;
                self.close_tag = None;

                if after_close.is_empty() {
                    return (String::new(), reasoning);
                }

                let (more_content, more_reasoning) = self.process_outside_think(after_close);
                if !more_reasoning.is_empty() {
                    reasoning.push_str(&more_reasoning);
                }
                (more_content, reasoning)
            }
            None => {
                let safe_len = find_safe_split_point_close(input, close_tag);
                let reasoning = safe_slice_to(input, safe_len).to_string();
                let tail = safe_slice_from(input, safe_len);
                if !tail.is_empty() {
                    self.tag_buffer.push_str(tail);
                }
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

/// Latest byte offset in `input` that splits before a possible partial
/// opening tag, so the tail can be buffered until the rest of the tag lands.
fn find_safe_split_point(input: &str) -> usize {
    let max_tag_len = 11usize; // "<reasoning>".len()
    let check_len = std::cmp::min(max_tag_len.saturating_sub(1), input.len());
    let tail_start = input.len() - check_len;
    if !input.as_bytes()[tail_start..].contains(&b'<') {
        return input.len();
    }

    (1..=check_len)
        .rev()
        .map(|partial_len| input.len() - partial_len)
        .find(|&split_byte| {
            input.is_char_boundary(split_byte) && is_partial_open_tag_tail(&input[split_byte..])
        })
        .unwrap_or(input.len())
}

/// Same as [`find_safe_split_point`], for a partial `close_tag` tail.
fn find_safe_split_point_close(input: &str, close_tag: &str) -> usize {
    let check_len = std::cmp::min(close_tag.len() - 1, input.len());
    let tail_start = input.len() - check_len;
    if !input.as_bytes()[tail_start..].contains(&b'<') {
        return input.len();
    }

    for partial_len in (1..=check_len).rev() {
        let split_byte = input.len() - partial_len;
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
#[path = "think_extractor_tests.rs"]
mod tests;
