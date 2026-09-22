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
        if calls.is_empty() { None } else { Some(calls) }
    }
}

/// Parses all `<invoke ...>...</invoke>`, `<function ...>...</function>`, `<call ...>...</call>`,
/// and `<function_call ...>...</function_call>` tags within the provided block.
pub fn parse_xml_invokes(text: &str) -> Vec<ParsedToolCall> {
    let mut calls = Vec::new();
    let mut cursor = 0;

    while cursor < text.len() {
        let remainder = &text[cursor..];
        let Some((tag_start, close_tag)) = find_next_invoke_tag(remainder) else {
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
        let explicit_id = extract_xml_attribute(open_tag_content, "id")
            .or_else(|| extract_xml_attribute(open_tag_content, "call_id"));

        let is_self_closing = open_tag_content[..open_tag_content.len().saturating_sub(1)]
            .trim_end()
            .ends_with('/');

        let (body, next_cursor) = if is_self_closing {
            ("", tag_body_start)
        } else {
            let after_body = &text[tag_body_start..];
            match find_ignore_ascii_case(after_body, close_tag) {
                Some(close_pos) => {
                    let body = &after_body[..close_pos];
                    let next = tag_body_start + close_pos + close_tag.len();
                    (body, next)
                }
                None => {
                    // If closing tag is missing (e.g. at end of stream), take remainder of block
                    (after_body, text.len())
                }
            }
        };

        if let Some(name) = func_name
            && !name.trim().is_empty()
        {
            let id = explicit_id.unwrap_or_else(generate_tool_call_id);
            let arguments = parse_invoke_to_json_arguments(open_tag_content, body);
            calls.push(ParsedToolCall {
                id,
                name: name.trim().to_string(),
                arguments,
            });
        }

        cursor = advance_cursor(text, cursor, next_cursor);
    }

    calls
}

/// Finds the next `<invoke`, `<function_call`, `<function`, `<call`, or `<action` tag start in `s`.
fn find_next_invoke_tag(s: &str) -> Option<(usize, &'static str)> {
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
            // Validate boundary after tag name: must be whitespace, '>' or '/'
            let next_byte = s.as_bytes().get(after_tag);
            let is_boundary =
                next_byte.is_none_or(|&b| b.is_ascii_whitespace() || b == b'>' || b == b'/');
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

/// Extract all attribute (name, value) pairs from an opening XML tag.
pub fn extract_all_attributes(tag_str: &str) -> Vec<(String, String)> {
    let mut attributes = Vec::new();
    let bytes = tag_str.as_bytes();
    let len = bytes.len();
    if len == 0 {
        return attributes;
    }

    // Skip tag name if starting with '<' or if tag name is present
    let mut i = 0;
    if bytes[0] == b'<' {
        i = 1;
        while i < len && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' && bytes[i] != b'/' {
            i += 1;
        }
    } else {
        let first_eq = tag_str.find('=');
        let first_ws = tag_str.find(char::is_whitespace);
        if let Some(ws) = first_ws
            && first_eq.is_none_or(|eq| ws < eq)
        {
            i = ws;
        }
    }

    while i < len {
        // Skip whitespace
        while i < len && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= len || bytes[i] == b'>' || bytes[i] == b'/' {
            break;
        }

        // Read attribute name
        let key_start = i;
        while i < len
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'>'
            && bytes[i] != b'/'
        {
            i += 1;
        }
        let key = tag_str[key_start..i].to_string();
        if key.is_empty() {
            break;
        }

        // Skip whitespace
        while i < len && bytes[i].is_ascii_whitespace() {
            i += 1;
        }

        let val = if i < len && bytes[i] == b'=' {
            i += 1; // skip '='
            while i < len && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < len {
                let quote = bytes[i];
                if quote == b'"' || quote == b'\'' {
                    i += 1; // skip opening quote
                    let val_start = i;
                    while i < len && bytes[i] != quote {
                        i += 1;
                    }
                    let raw_val = &tag_str[val_start..i];
                    if i < len && bytes[i] == quote {
                        i += 1; // skip closing quote
                    }
                    decode_xml_entities(raw_val)
                } else {
                    let val_start = i;
                    while i < len
                        && !bytes[i].is_ascii_whitespace()
                        && bytes[i] != b'>'
                        && bytes[i] != b'/'
                    {
                        i += 1;
                    }
                    let raw_val = &tag_str[val_start..i];
                    decode_xml_entities(raw_val)
                }
            } else {
                String::new()
            }
        } else {
            "true".to_string()
        };

        attributes.push((key, val));
    }

    attributes
}

/// Extract an XML attribute value from an opening tag string (e.g. `name="fetch_web_page"` or `name = '...'`).
pub fn extract_xml_attribute(tag_str: &str, attr_name: &str) -> Option<String> {
    extract_all_attributes(tag_str)
        .into_iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(attr_name))
        .map(|(_, v)| v)
}

fn extract_extra_attributes(tag_str: &str) -> Vec<(String, String)> {
    extract_all_attributes(tag_str)
        .into_iter()
        .filter(|(k, _)| {
            !k.eq_ignore_ascii_case("name")
                && !k.eq_ignore_ascii_case("function")
                && !k.eq_ignore_ascii_case("id")
                && !k.eq_ignore_ascii_case("call_id")
        })
        .collect()
}

pub(crate) fn advance_cursor(s: &str, current: usize, next: usize) -> usize {
    if next > current && next <= s.len() && s.is_char_boundary(next) {
        next
    } else {
        s[current..]
            .chars()
            .next()
            .map_or(s.len(), |c| current + c.len_utf8())
    }
}

/// Decode common XML entities (&amp;, &lt;, &gt;, &quot;, &apos;, and numeric entities).
pub fn decode_xml_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' {
            let mut entity = String::with_capacity(8);
            let mut matched = false;
            while let Some(&next_c) = chars.peek() {
                if next_c == ';' {
                    chars.next();
                    matched = true;
                    break;
                }
                if (next_c.is_alphanumeric() || next_c == '#')
                    && let Some(ch) = chars.next()
                {
                    entity.push(ch);
                    if entity.len() > 10 {
                        break;
                    }
                } else {
                    break;
                }
            }
            if matched {
                match entity.as_str() {
                    "amp" => out.push('&'),
                    "lt" => out.push('<'),
                    "gt" => out.push('>'),
                    "quot" => out.push('"'),
                    "apos" => out.push('\''),
                    s if s.starts_with("#x") || s.starts_with("#X") => {
                        if let Ok(code) = u32::from_str_radix(&s[2..], 16)
                            && let Some(ch) = char::from_u32(code)
                        {
                            out.push(ch);
                        } else {
                            out.push('&');
                            out.push_str(&entity);
                            out.push(';');
                        }
                    }
                    s if s.starts_with('#') => {
                        if let Ok(code) = s[1..].parse::<u32>()
                            && let Some(ch) = char::from_u32(code)
                        {
                            out.push(ch);
                        } else {
                            out.push('&');
                            out.push_str(&entity);
                            out.push(';');
                        }
                    }
                    _ => {
                        out.push('&');
                        out.push_str(&entity);
                        out.push(';');
                    }
                }
            } else {
                out.push('&');
                out.push_str(&entity);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Strips CDATA wrappers (`<![CDATA[...]]>`) from a string.
fn strip_cdata(s: &str) -> String {
    if !s.contains("<![CDATA[") {
        return s.to_string();
    }
    let mut result = s.to_string();
    while let Some(start) = result.find("<![CDATA[") {
        if let Some(end) = result[start + 9..].find("]]>") {
            let abs_end = start + 9 + end;
            let inner = result[start + 9..abs_end].to_string();
            result.replace_range(start..abs_end + 3, &inner);
        } else {
            break;
        }
    }
    result
}

fn parse_invoke_to_json_arguments(open_tag_content: &str, body: &str) -> String {
    let extra_attrs = extract_extra_attributes(open_tag_content);
    let trimmed_body = body.trim();

    if trimmed_body.is_empty() {
        if extra_attrs.is_empty() {
            return "{}".to_string();
        }
        let mut map = Map::new();
        for (k, v) in extra_attrs {
            map.insert(k, parse_scalar_value(&v));
        }
        return Value::Object(map).to_string();
    }

    let body_json = parse_invoke_body_to_json_arguments(trimmed_body);
    if extra_attrs.is_empty() {
        return body_json;
    }

    if let Ok(Value::Object(mut map)) = serde_json::from_str::<Value>(&body_json) {
        for (k, v) in extra_attrs {
            map.entry(k).or_insert_with(|| parse_scalar_value(&v));
        }
        Value::Object(map).to_string()
    } else {
        body_json
    }
}

/// Parses the inner body of an `<invoke>` tag into a JSON string representing arguments.
fn parse_invoke_body_to_json_arguments(body: &str) -> String {
    let trimmed = body.trim();

    // Case 1: Body is already a JSON object
    if trimmed.starts_with('{')
        && trimmed.ends_with('}')
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
    if inner_trimmed.starts_with('{')
        && inner_trimmed.ends_with('}')
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
        let (content, next_cursor) =
            match find_ignore_ascii_case(&inner_body[content_start..], &close_tag) {
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

        // If Anthropic style: <parameter name="key">value</parameter> or <param name="key">
        let key = if raw_tag_name.eq_ignore_ascii_case("parameter")
            || raw_tag_name.eq_ignore_ascii_case("param")
        {
            extract_xml_attribute(open_tag_header, "name")
                .unwrap_or_else(|| "parameter".to_string())
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
    let un_cdata = strip_cdata(text);
    let decoded = decode_xml_entities(&un_cdata);
    let trimmed = decoded.trim();
    if trimmed.is_empty() {
        return Value::String(String::new());
    }

    // Direct literals
    if trimmed.eq_ignore_ascii_case("true") {
        return Value::Bool(true);
    }
    if trimmed.eq_ignore_ascii_case("false") {
        return Value::Bool(false);
    }
    if trimmed.eq_ignore_ascii_case("null") {
        return Value::Null;
    }

    // Try parsing as valid JSON (numbers, arrays, nested objects)
    if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
        match val {
            Value::Number(_) | Value::Array(_) | Value::Object(_) => return val,
            Value::String(_) => return val,
            _ => {}
        }
    }

    Value::String(trimmed.to_string())
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
