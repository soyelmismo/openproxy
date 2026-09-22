pub mod hermes_json;
pub mod minimax_xml;
pub mod parser;
pub mod stream;
pub mod types;

#[cfg(test)]
mod tests;

pub use hermes_json::HermesJsonParser;
pub use minimax_xml::MiniMaxXmlParser;
pub use parser::InlineToolParser;
pub use stream::InlineToolStreamExtractor;
pub use types::{ExtractedInlineTools, ParsedToolCall, generate_tool_call_id};

use openproxy_types::{OpenAIChoice, OpenAIResponse};

/// Enclosing tag pairs recognized by the inline tool extractor.
const ENCLOSING_TAG_PAIRS: &[(&str, &str)] = &[
    ("<tool_call", "</tool_call>"),
    ("<tool_calls", "</tool_calls>"),
    ("<function_calls", "</function_calls>"),
    ("<tools", "</tools>"),
    ("<tool", "</tool>"),
];

/// Extracts inline tool calls from a text content string using all registered parsers.
///
/// Strips the inline tool call blocks from `content` and returns both the cleaned text
/// and the parsed structured tool calls.
pub fn extract_inline_tools(content: &str) -> ExtractedInlineTools {
    let mut all_calls = Vec::new();
    let mut spans_to_remove: Vec<(usize, usize)> = Vec::new();

    let xml_parser = MiniMaxXmlParser::new();
    let json_parser = HermesJsonParser::new();

    // 1. Look for enclosing tag pairs: <tool_call>...</tool_call>, etc. (allowing attributes)
    let mut cursor = 0;
    while cursor < content.len() {
        let remainder = &content[cursor..];
        let Some((open_tag_start, open_tag_end, close_tag)) = find_earliest_enclosing_block(remainder) else {
            break;
        };

        let abs_body_start = cursor + open_tag_end;
        let abs_open = cursor + open_tag_start;
        let open_tag = &content[abs_open..abs_body_start];
        let is_self_closing = open_tag[..open_tag.len().saturating_sub(1)]
            .trim_end()
            .ends_with('/');

        let (block_content, abs_close) = if is_self_closing {
            (open_tag, abs_body_start)
        } else {
            let after_open = &content[abs_body_start..];
            match find_ignore_ascii_case(after_open, close_tag) {
                Some(close_pos) => {
                    let body = &after_open[..close_pos];
                    let close = abs_body_start + close_pos + close_tag.len();
                    (body, close)
                }
                None => (after_open, content.len()),
            }
        };

        // Try XML parser first (MiniMax / Anthropic format)
        let parsed_calls = xml_parser
            .parse_block(block_content)
            // Then Hermes JSON format
            .or_else(|| json_parser.parse_block(block_content));

        if let Some(calls) = parsed_calls {
            all_calls.extend(calls);
            spans_to_remove.push((abs_open, abs_close));
        }

        cursor = minimax_xml::advance_cursor(content, cursor, abs_close);
    }

    // 2. Look for [TOOL_CALLS] blocks
    cursor = 0;
    while cursor < content.len() {
        let remainder = &content[cursor..];
        let Some(pos) = find_ignore_ascii_case(remainder, "[TOOL_CALLS]") else {
            break;
        };

        let abs_start = cursor + pos;
        let after_marker = &content[abs_start + "[TOOL_CALLS]".len()..];

        // The JSON array/object should start near the marker
        if let Some(json_start_rel) = after_marker.find(['[', '{']) {
            let json_body = after_marker[json_start_rel..].trim_start();
            if let Some(calls) = json_parser.parse_block(json_body) {
                all_calls.extend(calls);
                spans_to_remove.push((abs_start, content.len()));
                break;
            }
        }

        cursor = minimax_xml::advance_cursor(content, cursor, abs_start + "[TOOL_CALLS]".len());
    }

    // 3. If no enclosing tags were matched, search for bare <invoke ...>...</invoke>, <function ...>, etc.
    if spans_to_remove.is_empty() {
        let bare_calls = minimax_xml::parse_xml_invokes(content);
        if !bare_calls.is_empty() {
            // Find and strip individual invocation blocks
            let mut search_cursor = 0;
            while search_cursor < content.len() {
                let rem = &content[search_cursor..];
                let Some((start_rel, close_tag)) = find_bare_invoke_span(rem) else {
                    break;
                };
                let abs_start = search_cursor + start_rel;
                let body_start = match content[abs_start..].find('>') {
                    Some(p) => abs_start + p + 1,
                    None => break,
                };
                let open_tag = &content[abs_start..body_start];
                let is_self_closing = open_tag[..open_tag.len().saturating_sub(1)]
                    .trim_end()
                    .ends_with('/');
                let end = if is_self_closing {
                    body_start
                } else {
                    match find_ignore_ascii_case(&content[body_start..], close_tag) {
                        Some(cp) => body_start + cp + close_tag.len(),
                        None => content.len(),
                    }
                };
                spans_to_remove.push((abs_start, end));
                search_cursor = minimax_xml::advance_cursor(content, search_cursor, end);
            }
            all_calls = bare_calls;
        }
    }

    if spans_to_remove.is_empty() {
        return ExtractedInlineTools {
            clean_content: content.to_string(),
            tool_calls: Vec::new(),
        };
    }

    // Merge overlapping/contiguous spans
    spans_to_remove.sort_by_key(|&(s, _)| s);
    let mut merged_spans: Vec<(usize, usize)> = Vec::new();
    for (start, end) in spans_to_remove {
        if let Some(last) = merged_spans.last_mut()
            && start <= last.1
        {
            last.1 = last.1.max(end);
            continue;
        }
        merged_spans.push((start, end));
    }

    // Construct clean_content
    let mut clean = String::with_capacity(content.len());
    let mut last_idx = 0;
    for (start, end) in merged_spans {
        if start > last_idx {
            let part = safe_slice(content, last_idx, start);
            if clean.ends_with(char::is_whitespace) && part.starts_with(char::is_whitespace) {
                clean.push_str(part.trim_start());
            } else {
                clean.push_str(part);
            }
        }
        last_idx = end;
    }
    if last_idx < content.len() {
        let part = safe_slice(content, last_idx, content.len());
        if clean.ends_with(char::is_whitespace) && part.starts_with(char::is_whitespace) {
            clean.push_str(part.trim_start());
        } else {
            clean.push_str(part);
        }
    }

    let trimmed = clean.trim().to_string();

    ExtractedInlineTools {
        clean_content: trimmed,
        tool_calls: all_calls,
    }
}

/// Extracts inline tool calls from an assistant choice message.
pub fn extract_inline_tools_from_choice(choice: &mut OpenAIChoice) {
    if choice.message.role != "assistant" {
        return;
    }
    let raw_text = choice.message.extract_text();
    if raw_text.is_empty() {
        return;
    }
    let extracted = extract_inline_tools(&raw_text);
    if !extracted.has_tools() {
        return;
    }

    if extracted.clean_content.is_empty() {
        choice.message.content = None;
    } else {
        choice.message.content = Some(serde_json::Value::String(extracted.clean_content));
    }

    let mut existing_tools = choice.message.tool_calls.take().unwrap_or_default();
    for tc in extracted.tool_calls {
        existing_tools.push(tc.to_openai_value());
    }
    choice.message.tool_calls = Some(existing_tools);
    choice.finish_reason = Some("tool_calls".to_string());
}

/// Post-processes an entire [`OpenAIResponse`] to extract inline tool calls across all choices.
pub fn extract_inline_tools_from_response(mut resp: OpenAIResponse) -> OpenAIResponse {
    for choice in &mut resp.choices {
        extract_inline_tools_from_choice(choice);
    }
    resp
}

fn find_earliest_enclosing_block(s: &str) -> Option<(usize, usize, &'static str)> {
    let mut earliest: Option<(usize, usize, &'static str)> = None;
    for &(open, close) in ENCLOSING_TAG_PAIRS {
        let mut cursor = 0;
        while let Some(pos) = find_ignore_ascii_case(&s[cursor..], open) {
            let abs_pos = cursor + pos;
            let after_tag = abs_pos + open.len();
            let next_byte = s.as_bytes().get(after_tag);
            let is_boundary = next_byte.is_none_or(|&b| b.is_ascii_whitespace() || b == b'>' || b == b'/');
            if is_boundary {
                // Find closing '>' of opening tag
                if let Some(tag_end) = s[abs_pos..].find('>') {
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

fn find_bare_invoke_span(s: &str) -> Option<(usize, &'static str)> {
    let candidates = [
        ("<invoke", "</invoke>"),
        ("<function_call", "</function_call>"),
        ("<function", "</function>"),
        ("<call", "</call>"),
        ("<action", "</action>"),
    ];
    let mut best: Option<(usize, &'static str)> = None;
    for (open, close) in candidates {
        let mut cursor = 0;
        while let Some(pos) = find_ignore_ascii_case(&s[cursor..], open) {
            let abs_pos = cursor + pos;
            let after_tag = abs_pos + open.len();
            let next_byte = s.as_bytes().get(after_tag);
            let is_boundary = next_byte.is_none_or(|&b| b.is_ascii_whitespace() || b == b'>' || b == b'/');
            if is_boundary {
                if best.is_none_or(|(p, _)| abs_pos < p) {
                    best = Some((abs_pos, close));
                }
                break;
            }
            cursor = abs_pos + 1;
        }
    }
    best
}

#[inline]
fn safe_slice(s: &str, start: usize, end: usize) -> &str {
    let mut start_idx = std::cmp::min(start, s.len());
    while start_idx < s.len() && !s.is_char_boundary(start_idx) {
        start_idx += 1;
    }
    let mut end_idx = std::cmp::min(end, s.len());
    while end_idx > 0 && !s.is_char_boundary(end_idx) {
        end_idx -= 1;
    }
    if start_idx <= end_idx {
        &s[start_idx..end_idx]
    } else {
        ""
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
