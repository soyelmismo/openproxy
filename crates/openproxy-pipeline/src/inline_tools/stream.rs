use super::hermes_json::HermesJsonParser;
use super::minimax_xml::MiniMaxXmlParser;
use super::parser::InlineToolParser;
use super::types::ParsedToolCall;
use crate::streaming::{StreamAction, StreamingChunkStage};
use serde_json::Value;

const TOOL_TAG_CANDIDATES: &[(&str, &str)] = &[
    ("<tool_call", "</tool_call>"),
    ("<tool_calls", "</tool_calls>"),
    ("<function_calls", "</function_calls>"),
    ("<tools", "</tools>"),
    ("<tool", "</tool>"),
    ("<invoke", "</invoke>"),
    ("<function_call", "</function_call>"),
    ("<function", "</function>"),
    ("<call", "</call>"),
    ("<action", "</action>"),
];

const MAX_TOOL_BUFFER_BYTES: usize = 262_144; // 256 KiB safety cap

/// Streaming state machine that intercepts inline `<tool_call>` blocks
/// emitted in chunk `content` deltas and converts them into structured
/// `tool_calls` SSE chunks.
#[derive(Debug, Default, Clone)]
pub struct InlineToolStreamExtractor {
    /// True while buffering inside a tool call block.
    pub inside_tool_call: bool,
    /// The expected closing tag string for the current block (e.g. "</tool_call>", "</invoke>", etc.).
    pub expected_close_tag: Option<String>,
    /// Accumulated text of the current tool call block.
    pub tool_buffer: String,
    /// Trailing partial tag prefix buffer across chunk boundaries (e.g. "<tool_").
    pub partial_tag_buffer: String,
    /// True if we have emitted at least one structured tool call in this stream.
    pub emitted_tool_call: bool,
    /// Running index for streamed tool calls.
    pub tool_call_index: u32,
}

impl InlineToolStreamExtractor {
    pub fn new() -> Self {
        Self::default()
    }

    fn build_tool_calls_value(&mut self, calls: Vec<ParsedToolCall>) -> Vec<Value> {
        let tc_array: Vec<Value> = calls
            .into_iter()
            .enumerate()
            .map(|(i, tc)| {
                serde_json::json!({
                    "index": self.tool_call_index + i as u32,
                    "id": tc.id,
                    "type": "function",
                    "function": {
                        "name": tc.name,
                        "arguments": tc.arguments,
                    }
                })
            })
            .collect();
        self.tool_call_index += tc_array.len() as u32;
        tc_array
    }
}

impl StreamingChunkStage for InlineToolStreamExtractor {
    fn process_chunk(&mut self, payload: &str) -> StreamAction {
        // Fast path: if idle, empty buffer, and no tool markers or partial tags
        if !self.inside_tool_call
            && self.tool_buffer.is_empty()
            && self.partial_tag_buffer.is_empty()
            && !payload.contains('<')
            && !payload.contains("[TOOL_CALLS]")
        {
            if self.emitted_tool_call
                && (payload.contains("\"finish_reason\":\"stop\"")
                    || payload.contains("\"finish_reason\": \"stop\"")
                    || payload.contains("\"finish_reason\":\"end_turn\"")
                    || payload.contains("\"finish_reason\": \"end_turn\""))
            {
                return StreamAction::Mutate(
                    payload
                        .replace("\"finish_reason\":\"stop\"", "\"finish_reason\":\"tool_calls\"")
                        .replace("\"finish_reason\": \"stop\"", "\"finish_reason\": \"tool_calls\"")
                        .replace("\"finish_reason\":\"end_turn\"", "\"finish_reason\":\"tool_calls\"")
                        .replace("\"finish_reason\": \"end_turn\"", "\"finish_reason\": \"tool_calls\""),
                );
            }
            return StreamAction::Passthrough;
        }

        let Ok(mut chunk_val) = serde_json::from_str::<Value>(payload) else {
            return StreamAction::Passthrough;
        };

        // Check finish_reason
        let is_stop = chunk_val
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(|c| c.get("finish_reason"))
            .and_then(Value::as_str)
            .is_some_and(|fr| fr == "stop" || fr == "end_turn");

        let has_content_delta = chunk_val
            .pointer("/choices/0/delta/content")
            .and_then(Value::as_str)
            .is_some();

        if is_stop
            && self.emitted_tool_call
            && !has_content_delta
            && let Some(choice) = chunk_val.pointer_mut("/choices/0")
        {
            choice["finish_reason"] = Value::String("tool_calls".to_string());
            if let Ok(mutated) = serde_json::to_string(&chunk_val) {
                return StreamAction::Mutate(mutated);
            }
        }

        let Some(choice) = chunk_val
            .get_mut("choices")
            .and_then(Value::as_array_mut)
            .and_then(|arr| arr.first_mut())
        else {
            return StreamAction::Passthrough;
        };

        let Some(delta) = choice.get_mut("delta") else {
            return StreamAction::Passthrough;
        };

        let Some(content_val) = delta.get("content").and_then(Value::as_str) else {
            return StreamAction::Passthrough;
        };

        let raw_chunk_content = content_val.to_string();
        let full_content = if !self.partial_tag_buffer.is_empty() {
            let mut s = std::mem::take(&mut self.partial_tag_buffer);
            s.push_str(&raw_chunk_content);
            s
        } else {
            raw_chunk_content
        };

        let mut chunk_tool_calls = Vec::new();
        let mut clean_text = String::new();
        let mut text_to_process = String::new();

        if self.inside_tool_call {
            self.tool_buffer.push_str(&full_content);
            if self.tool_buffer.len() > MAX_TOOL_BUFFER_BYTES {
                clean_text.push_str(&std::mem::take(&mut self.tool_buffer));
                self.inside_tool_call = false;
                self.expected_close_tag = None;
            } else {
                let close_tag = self
                    .expected_close_tag
                    .clone()
                    .unwrap_or_else(|| "</tool_call>".to_string());

                if let Some(close_pos) = find_close_tag_in_buffer(&self.tool_buffer, &close_tag) {
                    let tool_block = self.tool_buffer[..close_pos].to_string();
                    let remainder = self.tool_buffer[close_pos..].to_string();
                    self.tool_buffer.clear();
                    self.inside_tool_call = false;
                    self.expected_close_tag = None;

                    if let Some(calls) = parse_tool_block(&tool_block) {
                        self.emitted_tool_call = true;
                        chunk_tool_calls.extend(calls);
                    } else {
                        clean_text.push_str(&tool_block);
                    }
                    text_to_process = remainder;
                } else {
                    return StreamAction::Skip;
                }
            }
        } else {
            text_to_process = full_content;
        }

        let mut cursor = 0;
        while cursor < text_to_process.len() {
            let slice = &text_to_process[cursor..];
            if let Some((open_start, open_end, close_tag)) = find_earliest_tool_tag(slice) {
                if open_start > 0 {
                    append_clean_text(&mut clean_text, &slice[..open_start]);
                }
                let open_tag_slice = &slice[open_start..open_end];
                let is_self_closing = open_tag_slice[..open_tag_slice.len().saturating_sub(1)]
                    .trim_end()
                    .ends_with('/');

                if is_self_closing {
                    let tool_block = open_tag_slice;
                    if let Some(calls) = parse_tool_block(tool_block) {
                        self.emitted_tool_call = true;
                        chunk_tool_calls.extend(calls);
                    } else {
                        append_clean_text(&mut clean_text, tool_block);
                    }
                    cursor += open_end;
                } else {
                    let after_open = &slice[open_end..];
                    if let Some(close_pos) = find_close_tag_in_buffer(after_open, close_tag) {
                        let full_tool_end = open_end + close_pos;
                        let tool_block = &slice[open_start..full_tool_end];
                        if let Some(calls) = parse_tool_block(tool_block) {
                            self.emitted_tool_call = true;
                            chunk_tool_calls.extend(calls);
                        } else {
                            append_clean_text(&mut clean_text, tool_block);
                        }
                        cursor += full_tool_end;
                    } else {
                        self.inside_tool_call = true;
                        self.expected_close_tag = Some(close_tag.to_string());
                        self.tool_buffer.push_str(&slice[open_start..]);
                        break;
                    }
                }
            } else {
                let (safe, partial) = extract_trailing_partial_tag_prefix(slice);
                if !safe.is_empty() {
                    append_clean_text(&mut clean_text, safe);
                }
                if !partial.is_empty() {
                    self.partial_tag_buffer = partial.to_string();
                }
                break;
            }
        }

        if !chunk_tool_calls.is_empty() {
            let tc_array = self.build_tool_calls_value(chunk_tool_calls);
            let trimmed_clean = clean_text.trim();
            if let Some(obj) = delta.as_object_mut() {
                if trimmed_clean.is_empty() {
                    obj.remove("content");
                } else {
                    obj.insert(
                        "content".to_string(),
                        Value::String(trimmed_clean.to_string()),
                    );
                }
                obj.insert("tool_calls".to_string(), Value::Array(tc_array));
            }
            if choice.get("finish_reason").is_some() {
                choice["finish_reason"] = Value::String("tool_calls".to_string());
            }
            if let Ok(mutated) = serde_json::to_string(&chunk_val) {
                return StreamAction::Mutate(mutated);
            }
        } else if self.inside_tool_call || !self.partial_tag_buffer.is_empty() {
            if clean_text.is_empty() {
                return StreamAction::Skip;
            }
            delta["content"] = Value::String(clean_text);
            if let Ok(mutated) = serde_json::to_string(&chunk_val) {
                return StreamAction::Mutate(mutated);
            }
        } else if is_stop && self.emitted_tool_call {
            choice["finish_reason"] = Value::String("tool_calls".to_string());
            if let Ok(mutated) = serde_json::to_string(&chunk_val) {
                return StreamAction::Mutate(mutated);
            }
        } else if clean_text != content_val {
            delta["content"] = Value::String(clean_text);
            if let Ok(mutated) = serde_json::to_string(&chunk_val) {
                return StreamAction::Mutate(mutated);
            }
        }

        StreamAction::Passthrough
    }

    fn finalize(&mut self) -> Option<String> {
        let mut residual = std::mem::take(&mut self.partial_tag_buffer);
        residual.push_str(&self.tool_buffer);
        self.tool_buffer.clear();
        self.inside_tool_call = false;
        self.expected_close_tag = None;

        if residual.trim().is_empty() {
            return None;
        }

        // Try parsing whatever is left
        if let Some(calls) = parse_tool_block(&residual) {
            self.emitted_tool_call = true;
            let tc_array = self.build_tool_calls_value(calls);
            let chunk = serde_json::json!({
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": tc_array
                    },
                    "finish_reason": "tool_calls"
                }]
            });
            return serde_json::to_string(&chunk).ok();
        }

        // If unparseable, flush remaining buffer as plain content
        let chunk = serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": {
                    "content": residual
                },
                "finish_reason": null
            }]
        });
        serde_json::to_string(&chunk).ok()
    }
}

fn parse_tool_block(block: &str) -> Option<Vec<ParsedToolCall>> {
    let trimmed = block.trim();
    if trimmed.is_empty() {
        return None;
    }
    let xml_parser = MiniMaxXmlParser::new();
    if let Some(calls) = xml_parser.parse_block(trimmed) {
        return Some(calls);
    }
    let inner = strip_tool_call_tags(trimmed);
    let json_parser = HermesJsonParser::new();
    if let Some(calls) = json_parser.parse_block(inner) {
        return Some(calls);
    }
    None
}

fn find_earliest_tool_tag(input: &str) -> Option<(usize, usize, &'static str)> {
    let mut earliest: Option<(usize, usize, &'static str)> = None;

    if let Some(pos) = find_ignore_ascii_case(input, "[TOOL_CALLS]") {
        earliest = Some((pos, pos + "[TOOL_CALLS]".len(), "]"));
    }

    for &(open, close) in TOOL_TAG_CANDIDATES {
        let mut cursor = 0;
        while let Some(pos) = find_ignore_ascii_case(&input[cursor..], open) {
            let abs_pos = cursor + pos;
            let after_tag = abs_pos + open.len();
            let next_byte = input.as_bytes().get(after_tag);
            let is_boundary = next_byte.is_none_or(|&b| b.is_ascii_whitespace() || b == b'>' || b == b'/');
            if is_boundary {
                if let Some(tag_end) = input[abs_pos..].find('>') {
                    let full_open_end = abs_pos + tag_end + 1;
                    if earliest.is_none_or(|(p, _, _)| abs_pos < p) {
                        earliest = Some((abs_pos, full_open_end, close));
                    }
                }
                break;
            }
            cursor = abs_pos + 1;
        }
    }
    earliest
}

fn find_close_tag_in_buffer(buffer: &str, expected_close: &str) -> Option<usize> {
    if expected_close == "]" {
        if let Some(pos) = buffer.rfind(']') {
            return Some(pos + 1);
        }
        if let Some(pos) = buffer.rfind('}') {
            return Some(pos + 1);
        }
        return None;
    }
    find_ignore_ascii_case(buffer, expected_close).map(|p| p + expected_close.len())
}

fn extract_trailing_partial_tag_prefix(s: &str) -> (&str, &str) {
    if s.is_empty() {
        return (s, "");
    }
    let last_lt = s.rfind('<');
    let last_bracket = s.rfind('[');
    let candidate_start = match (last_lt, last_bracket) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };

    if let Some(start) = candidate_start {
        let suffix = &s[start..];
        if !suffix.contains('>') && !suffix.contains(']') && suffix.len() <= 1024 {
            let suffix_lower = suffix.to_ascii_lowercase();
            let matches_prefix = TOOL_TAG_CANDIDATES.iter().any(|(open, _)| {
                let open_lower = open.to_ascii_lowercase();
                if open_lower.starts_with(&suffix_lower) {
                    return true;
                }
                if suffix_lower.starts_with(&open_lower) {
                    let after_tag = &suffix[open.len()..];
                    let next_b = after_tag.as_bytes().first();
                    return next_b.is_none_or(|&b| b.is_ascii_whitespace() || b == b'/' || b == b'>');
                }
                false
            }) || {
                let marker = "[tool_calls]";
                marker.starts_with(&suffix_lower) || suffix_lower.starts_with(marker)
            };
            if matches_prefix {
                return (&s[..start], suffix);
            }
        }
    }
    (s, "")
}

fn append_clean_text(clean: &mut String, part: &str) {
    if clean.ends_with(char::is_whitespace) && part.starts_with(char::is_whitespace) {
        clean.push_str(part.trim_start());
    } else {
        clean.push_str(part);
    }
}

fn strip_tool_call_tags(s: &str) -> &str {
    let mut trimmed = s.trim();
    if let Some(pos) = find_ignore_ascii_case(trimmed, "[TOOL_CALLS]")
        && pos == 0
    {
        trimmed = trimmed["[TOOL_CALLS]".len()..].trim();
    }
    for &(open, _) in TOOL_TAG_CANDIDATES {
        if let Some(pos) = find_ignore_ascii_case(trimmed, open)
            && pos == 0
            && let Some(gt_pos) = trimmed.find('>')
        {
            trimmed = trimmed[gt_pos + 1..].trim();
            break;
        }
    }
    for &(_, close) in TOOL_TAG_CANDIDATES {
        if let Some(pos) = find_ignore_ascii_case(trimmed, close) {
            trimmed = trimmed[..pos].trim();
            break;
        }
    }
    trimmed
}

fn find_ignore_ascii_case(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if haystack.len() < needle.len() {
        return None;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}
