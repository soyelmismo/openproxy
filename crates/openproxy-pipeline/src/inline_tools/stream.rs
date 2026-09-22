use super::minimax_xml::MiniMaxXmlParser;
use super::hermes_json::HermesJsonParser;
use super::parser::InlineToolParser;
use super::types::ParsedToolCall;
use crate::streaming::{StreamAction, StreamingChunkStage};
use serde_json::Value;

/// Streaming state machine that intercepts inline `<tool_call>` blocks
/// emitted in chunk `content` deltas and converts them into structured
/// `tool_calls` SSE chunks.
#[derive(Debug, Default, Clone)]
pub struct InlineToolStreamExtractor {
    /// True while buffering inside a tool call block.
    pub inside_tool_call: bool,
    /// Accumulated text of the current tool call block.
    pub tool_buffer: String,
    /// True if we have emitted at least one structured tool call in this stream.
    pub emitted_tool_call: bool,
    /// Running index for streamed tool calls.
    pub tool_call_index: u32,
    /// Pending tool call chunks to emit.
    pub pending_emits: Vec<String>,
}

impl InlineToolStreamExtractor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Tries to parse the buffered tool call text using available parsers.
    fn parse_buffered_tools(&self) -> Option<Vec<ParsedToolCall>> {
        let trimmed = self.tool_buffer.trim();
        if trimmed.is_empty() {
            return None;
        }

        // Try MiniMax XML
        let xml_parser = MiniMaxXmlParser::new();
        if let Some(calls) = xml_parser.parse_block(trimmed) {
            return Some(calls);
        }

        // Try Hermes JSON (strip enclosing <tool_call> tags if present)
        let inner = strip_tool_call_tags(trimmed);
        let json_parser = HermesJsonParser::new();
        if let Some(calls) = json_parser.parse_block(inner) {
            return Some(calls);
        }

        None
    }
}

/// Tags that mark the start of an inline tool call.
const TOOL_CALL_OPEN_TAGS: &[&str] = &["<tool_call>", "<tool_calls>", "<function_calls>", "<invoke "];
/// Matching close tags.
const TOOL_CALL_CLOSE_TAGS: &[&str] = &["</tool_call>", "</tool_calls>", "</function_calls>", "</invoke>"];

impl StreamingChunkStage for InlineToolStreamExtractor {
    fn process_chunk(&mut self, payload: &str) -> StreamAction {
        // Fast path: if idle and no tool tags, passthrough directly
        if !self.inside_tool_call
            && self.tool_buffer.is_empty()
            && !payload.contains("<tool_call")
            && !payload.contains("<tool_calls")
            && !payload.contains("<function_calls")
            && !payload.contains("<invoke")
            && !payload.contains("[TOOL_CALLS]")
        {
            // If this is a finish_reason chunk and we previously emitted tool calls,
            // patch finish_reason to "tool_calls".
            if self.emitted_tool_call && payload.contains("\"finish_reason\":\"stop\"") {
                return StreamAction::Mutate(payload.replace("\"finish_reason\":\"stop\"", "\"finish_reason\":\"tool_calls\""));
            }
            return StreamAction::Passthrough;
        }

        // Parse chunk JSON to inspect delta
        let Ok(mut chunk_val) = serde_json::from_str::<Value>(payload) else {
            return StreamAction::Passthrough;
        };

        let is_stop = chunk_val
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(|c| c.get("finish_reason"))
            .and_then(Value::as_str)
            .is_some_and(|fr| fr == "stop");

        if is_stop
            && self.emitted_tool_call
            && let Some(choices) = chunk_val.get_mut("choices").and_then(Value::as_array_mut)
            && let Some(choice) = choices.first_mut()
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

        let content = content_val.to_string();

        if !self.inside_tool_call {
            if let Some((open_pos, _tag)) = find_earliest_tool_tag(&content) {
                let text_before = &content[..open_pos];
                let tool_part = &content[open_pos..];
                self.tool_buffer.push_str(tool_part);
                self.inside_tool_call = true;

                // Check if closing tag is already in buffer
                if let Some(close_pos) = find_tool_close_tag(&self.tool_buffer) {
                    let full_tool_block = self.tool_buffer[..close_pos].to_string();
                    let _text_after = self.tool_buffer[close_pos..].to_string();
                    self.tool_buffer = full_tool_block;

                    let parsed = self.parse_buffered_tools();
                    self.tool_buffer.clear();
                    self.inside_tool_call = false;

                    if let Some(calls) = parsed {
                        self.emitted_tool_call = true;
                        // Replace content delta with tool_calls delta
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

                        let delta_obj = delta.as_object_mut();
                        if let Some(obj) = delta_obj {
                            obj.remove("content");
                            obj.insert("tool_calls".to_string(), Value::Array(tc_array));
                        }

                        // If there was text_before or text_after, emit as mutated payload
                        if let Ok(mutated) = serde_json::to_string(&chunk_val) {
                            return StreamAction::Mutate(mutated);
                        }
                    } else if !text_before.is_empty() {
                        delta["content"] = Value::String(text_before.to_string());
                        if let Ok(mutated) = serde_json::to_string(&chunk_val) {
                            return StreamAction::Mutate(mutated);
                        }
                    }
                } else if !text_before.is_empty() {
                    // Emit content before <tool_call> and buffer the rest
                    delta["content"] = Value::String(text_before.to_string());
                    if let Ok(mutated) = serde_json::to_string(&chunk_val) {
                        return StreamAction::Mutate(mutated);
                    }
                } else {
                    // Suppress chunk because it's purely opening tool call tag
                    return StreamAction::Skip;
                }
            }
        } else {
            // Inside tool call: accumulate into buffer
            self.tool_buffer.push_str(&content);

            if let Some(close_pos) = find_tool_close_tag(&self.tool_buffer) {
                let full_tool_block = self.tool_buffer[..close_pos].to_string();
                self.tool_buffer = full_tool_block;

                let parsed = self.parse_buffered_tools();
                self.tool_buffer.clear();
                self.inside_tool_call = false;

                if let Some(calls) = parsed {
                    self.emitted_tool_call = true;
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

                    let delta_obj = delta.as_object_mut();
                    if let Some(obj) = delta_obj {
                        obj.remove("content");
                        obj.insert("tool_calls".to_string(), Value::Array(tc_array));
                    }
                    if let Ok(mutated) = serde_json::to_string(&chunk_val) {
                        return StreamAction::Mutate(mutated);
                    }
                }
            }

            // Suppress intermediate tool call body chunks
            return StreamAction::Skip;
        }

        StreamAction::Passthrough
    }

    fn finalize(&mut self) -> Option<String> {
        if self.tool_buffer.is_empty() {
            return None;
        }

        // Try parsing whatever is left
        if let Some(calls) = self.parse_buffered_tools() {
            self.emitted_tool_call = true;
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
            let chunk = serde_json::json!({
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": tc_array
                    },
                    "finish_reason": "tool_calls"
                }]
            });
            self.tool_buffer.clear();
            self.inside_tool_call = false;
            return serde_json::to_string(&chunk).ok();
        }

        // If unparseable, flush remaining buffer as plain content
        let remaining = std::mem::take(&mut self.tool_buffer);
        self.inside_tool_call = false;
        let chunk = serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": {
                    "content": remaining
                },
                "finish_reason": null
            }]
        });
        serde_json::to_string(&chunk).ok()
    }
}

fn find_earliest_tool_tag(input: &str) -> Option<(usize, &'static str)> {
    let mut earliest: Option<(usize, &'static str)> = None;
    for &tag in TOOL_CALL_OPEN_TAGS {
        if let Some(pos) = find_ignore_ascii_case(input, tag)
            && earliest.is_none_or(|(p, _)| pos < p)
        {
            earliest = Some((pos, tag));
        }
    }
    earliest
}

fn find_tool_close_tag(buffer: &str) -> Option<usize> {
    for &tag in TOOL_CALL_CLOSE_TAGS {
        if let Some(pos) = find_ignore_ascii_case(buffer, tag) {
            return Some(pos + tag.len());
        }
    }
    None
}

fn strip_tool_call_tags(s: &str) -> &str {
    let mut trimmed = s.trim();
    for &tag in TOOL_CALL_OPEN_TAGS {
        if let Some(stripped) = strip_prefix_ignore_ascii_case(trimmed, tag) {
            trimmed = stripped.trim();
            break;
        }
    }
    for &tag in TOOL_CALL_CLOSE_TAGS {
        if let Some(stripped) = strip_suffix_ignore_ascii_case(trimmed, tag) {
            trimmed = stripped.trim();
            break;
        }
    }
    trimmed
}

fn strip_prefix_ignore_ascii_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes()) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

fn strip_suffix_ignore_ascii_case<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    if s.len() >= suffix.len() && s.as_bytes()[s.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes()) {
        Some(&s[..s.len() - suffix.len()])
    } else {
        None
    }
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
