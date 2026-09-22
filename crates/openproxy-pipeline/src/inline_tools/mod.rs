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

const ENCLOSING_TAG_PAIRS: &[(&str, &str)] = &[
    ("<tool_call>", "</tool_call>"),
    ("<tool_calls>", "</tool_calls>"),
    ("<function_calls>", "</function_calls>"),
    ("<tools>", "</tools>"),
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

    // 1. Look for enclosing tag pairs: <tool_call>...</tool_call>, etc.
    let mut cursor = 0;
    while cursor < content.len() {
        let remainder = &content[cursor..];
        let Some((open_tag, close_tag, open_pos)) = find_earliest_enclosing_pair(remainder) else {
            break;
        };

        let abs_open = cursor + open_pos;
        let after_open = &content[abs_open + open_tag.len()..];

        let (block_content, abs_close) = match find_ignore_ascii_case(after_open, close_tag) {
            Some(close_pos) => {
                let body = &after_open[..close_pos];
                let close = abs_open + open_tag.len() + close_pos + close_tag.len();
                (body, close)
            }
            None => {
                // If closing tag missing, take remainder of string
                (after_open, content.len())
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

        cursor = abs_close.max(cursor + 1);
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

        cursor = abs_start + "[TOOL_CALLS]".len();
    }

    // 3. If no enclosing tags were matched, search for bare <invoke ...>...</invoke>
    if spans_to_remove.is_empty() && (content.contains("<invoke") || content.contains("<function_call")) {
        let calls = minimax_xml::parse_xml_invokes(content);
        if !calls.is_empty() {
            // Find start and end of bare invokes to strip them
            let first_idx = content.find("<invoke").or_else(|| content.find("<function_call"));
            let last_close = content.rfind("</invoke>").map(|p| p + 9)
                .or_else(|| content.rfind("</function_call>").map(|p| p + 16));

            if let (Some(start), Some(end)) = (first_idx, last_close)
                && start < end
            {
                spans_to_remove.push((start, end));
                all_calls = calls;
            }
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
            clean.push_str(safe_slice(content, last_idx, start));
        }
        last_idx = end;
    }
    if last_idx < content.len() {
        clean.push_str(safe_slice(content, last_idx, content.len()));
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
    let Some(serde_json::Value::String(raw_content)) = &choice.message.content else {
        return;
    };
    let extracted = extract_inline_tools(raw_content);
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

fn find_earliest_enclosing_pair(s: &str) -> Option<(&'static str, &'static str, usize)> {
    let mut earliest: Option<(&'static str, &'static str, usize)> = None;
    for &(open, close) in ENCLOSING_TAG_PAIRS {
        if let Some(pos) = find_ignore_ascii_case(s, open)
            && earliest.is_none_or(|(_, _, p)| pos < p)
        {
            earliest = Some((open, close, pos));
        }
    }
    earliest
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
