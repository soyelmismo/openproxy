use super::parser::InlineToolParser;
use super::types::{ParsedToolCall, generate_tool_call_id};
use serde_json::{Map, Value};

/// Parser for MiniMax and Anthropic-style XML invoke tool calls:
///
/// ```xml
/// <tool_call>
/// <invoke name="fetch_web_page"><url>https://example.com</url></invoke>
/// </tool_call>
/// ```
#[derive(Debug, Default, Clone, Copy)]
pub struct MiniMaxXmlParser;

impl MiniMaxXmlParser {
    pub fn new() -> Self {
        Self
    }
}

impl InlineToolParser for MiniMaxXmlParser {
    fn parse_block(&self, block: &str) -> Option<Vec<ParsedToolCall>> {
        let calls = parse_xml_invokes(block);
        if calls.is_empty() {
            None
        } else {
            Some(calls)
        }
    }
}

/// Parses all `<invoke ...>...</invoke>` and `<function_call ...>...</function_call>`
/// tags within the provided block.
pub fn parse_xml_invokes(text: &str) -> Vec<ParsedToolCall> {
    let mut calls = Vec::new();
    let mut cursor = 0;

    while cursor < text.len() {
        let remainder = &text[cursor..];
        let Some((tag_start, _tag_name, is_invoke)) = find_next_invoke_tag(remainder) else {
            break;
        };

        let abs_tag_start = cursor + tag_start;
        let tag_body_start = match remainder[tag_start..].find('>') {
            Some(pos) => abs_tag_start + pos + 1,
            None => break,
        };

        let open_tag_content = &text[abs_tag_start..tag_body_start];
        let func_name = extract_xml_attribute(open_tag_content, "name")
            .or_else(|| extract_xml_attribute(open_tag_content, "function"));
        let explicit_id = extract_xml_attribute(open_tag_content, "id");

        let close_tag = if is_invoke { "</invoke>" } else { "</function_call>" };
        let after_body = &text[tag_body_start..];
        let (body, next_cursor) = match find_ignore_ascii_case(after_body, close_tag) {
            Some(close_pos) => {
                let body = &after_body[..close_pos];
                let next = tag_body_start + close_pos + close_tag.len();
                (body, next)
            }
            None => {
                // If closing tag is missing (e.g. at end of stream), take remainder of block
                (after_body, text.len())
            }
        };

        if let Some(name) = func_name
            && !name.trim().is_empty()
        {
            let id = explicit_id.unwrap_or_else(generate_tool_call_id);
            let arguments = parse_invoke_body_to_json_arguments(body);
            calls.push(ParsedToolCall {
                id,
                name: name.trim().to_string(),
                arguments,
            });
        }

        cursor = next_cursor.max(cursor + 1);
    }

    calls
}

/// Finds the next `<invoke` or `<function_call` tag start in `s`.
fn find_next_invoke_tag(s: &str) -> Option<(usize, &'static str, bool)> {
    let invoke_idx = find_ignore_ascii_case(s, "<invoke");
    let func_idx = find_ignore_ascii_case(s, "<function_call");

    match (invoke_idx, func_idx) {
        (Some(i), Some(f)) if i <= f => Some((i, "invoke", true)),
        (Some(_), Some(f)) => Some((f, "function_call", false)),
        (Some(i), None) => Some((i, "invoke", true)),
        (None, Some(f)) => Some((f, "function_call", false)),
        (None, None) => None,
    }
}

/// Extract an XML attribute value from an opening tag string (e.g. `name="fetch_web_page"`).
fn extract_xml_attribute(tag_str: &str, attr_name: &str) -> Option<String> {
    let mut search_key = String::with_capacity(attr_name.len() + 1);
    search_key.push_str(attr_name);
    search_key.push('=');

    let key_pos = find_ignore_ascii_case(tag_str, &search_key)?;
    let after_equal = tag_str.get(key_pos + search_key.len()..)?.trim_start();
    let mut chars = after_equal.chars();
    let quote = chars.next()?;

    if quote == '"' || quote == '\'' {
        let end_quote_idx = after_equal[quote.len_utf8()..].find(quote)?;
        Some(after_equal[quote.len_utf8()..quote.len_utf8() + end_quote_idx].to_string())
    } else {
        // Unquoted attribute value
        let val: String = after_equal
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '>' && *c != '/')
            .collect();
        if val.is_empty() { None } else { Some(val) }
    }
}

/// Parses the inner body of an `<invoke>` tag into a JSON string representing arguments.
fn parse_invoke_body_to_json_arguments(body: &str) -> String {
    let trimmed = body.trim();

    // Case 1: Body is already a JSON object
    if trimmed.starts_with('{') && trimmed.ends_with('}')
        && let Ok(v) = serde_json::from_str::<Value>(trimmed)
        && v.is_object()
    {
        return trimmed.to_string();
    }

    // Case 2: Strip outer <parameters> or <arguments> wrapper if present
    let inner_body = strip_outer_wrapper_tag(trimmed, "parameters")
        .or_else(|| strip_outer_wrapper_tag(trimmed, "arguments"))
        .unwrap_or(trimmed);

    // If unwrapped body is a JSON object:
    let inner_trimmed = inner_body.trim();
    if inner_trimmed.starts_with('{') && inner_trimmed.ends_with('}')
        && let Ok(v) = serde_json::from_str::<Value>(inner_trimmed)
        && v.is_object()
    {
        return inner_trimmed.to_string();
    }

    // Case 3: Parse XML child tags into JSON map
    let mut map = Map::new();
    let mut cursor = 0;

    while cursor < inner_body.len() {
        let remainder = &inner_body[cursor..];
        let Some(open_tag_start) = remainder.find('<') else {
            break;
        };
        let open_tag_abs = cursor + open_tag_start;

        let after_open = &inner_body[open_tag_abs + 1..];
        let Some(open_tag_end) = after_open.find('>') else {
            break;
        };
        let open_tag_header = &after_open[..open_tag_end].trim();

        // Check for self-closing tag or closing tag
        if open_tag_header.starts_with('/') {
            cursor = open_tag_abs + 1 + open_tag_end + 1;
            continue;
        }

        let is_self_closing = open_tag_header.ends_with('/');
        let raw_tag_name = open_tag_header
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_end_matches('/');

        if raw_tag_name.is_empty() {
            cursor = open_tag_abs + 1 + open_tag_end + 1;
            continue;
        }

        let content_start = open_tag_abs + 1 + open_tag_end + 1;

        if is_self_closing {
            // Handle <param name="foo" value="bar"/>
            let key = extract_xml_attribute(open_tag_header, "name")
                .unwrap_or_else(|| raw_tag_name.to_string());
            let val = extract_xml_attribute(open_tag_header, "value")
                .map_or(Value::Null, |v| parse_scalar_value(&v));
            map.insert(key, val);
            cursor = content_start;
            continue;
        }

        let close_tag = format!("</{raw_tag_name}>");
        let (content, next_cursor) = match find_ignore_ascii_case(&inner_body[content_start..], &close_tag) {
            Some(close_pos) => {
                let c = &inner_body[content_start..content_start + close_pos];
                let next = content_start + close_pos + close_tag.len();
                (c, next)
            }
            None => {
                let c = &inner_body[content_start..];
                (c, inner_body.len())
            }
        };

        // If Anthropic style: <parameter name="key">value</parameter>
        let key = if raw_tag_name.eq_ignore_ascii_case("parameter") {
            extract_xml_attribute(open_tag_header, "name").unwrap_or_else(|| "parameter".to_string())
        } else {
            raw_tag_name.to_string()
        };

        let val = parse_scalar_value(content.trim());
        map.insert(key, val);

        cursor = next_cursor.max(cursor + 1);
    }

    if map.is_empty() {
        "{}".to_string()
    } else {
        Value::Object(map).to_string()
    }
}

/// Convert scalar content text to JSON Value (bool, number, null, JSON, or String).
fn parse_scalar_value(text: &str) -> Value {
    if text.is_empty() {
        return Value::String(String::new());
    }

    // Direct literals
    if text.eq_ignore_ascii_case("true") {
        return Value::Bool(true);
    }
    if text.eq_ignore_ascii_case("false") {
        return Value::Bool(false);
    }
    if text.eq_ignore_ascii_case("null") {
        return Value::Null;
    }

    // Try parsing as valid JSON (numbers, arrays, nested objects)
    // Note: Numbers with leading zeros like "0123" or strings like URLs with '?'
    // will fail serde_json::from_str and safely become Value::String.
    if let Ok(val) = serde_json::from_str::<Value>(text) {
        match val {
            Value::Number(_) | Value::Array(_) | Value::Object(_) => return val,
            Value::String(_) => return val,
            _ => {}
        }
    }

    Value::String(text.to_string())
}

/// Strips an outer wrapper tag like `<parameters>...</parameters>`.
fn strip_outer_wrapper_tag<'a>(s: &'a str, tag_name: &str) -> Option<&'a str> {
    let open_tag = format!("<{tag_name}>");
    let close_tag = format!("</{tag_name}>");

    let open_pos = find_ignore_ascii_case(s, &open_tag)?;
    let close_pos = find_ignore_ascii_case(s, &close_tag)?;

    if open_pos < close_pos {
        let inner_start = open_pos + open_tag.len();
        Some(&s[inner_start..close_pos])
    } else {
        None
    }
}

/// Case-insensitive needle search in haystack without allocation.
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
