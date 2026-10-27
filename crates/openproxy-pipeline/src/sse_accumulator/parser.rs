//! Parsers and text extraction helpers for SSE streaming chunks.

use serde_json::Value;

use super::types::{ToolCallProbe, ToolCallProbeOuter};

/// Decode standard JSON string escape sequences into the destination byte buffer.
pub fn decode_json_escape_into(raw: &str, out: &mut Vec<u8>) {
    let bytes = raw.as_bytes();
    if !bytes.contains(&b'\\') {
        out.extend_from_slice(bytes);
        return;
    }
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'"' => {
                    out.push(b'"');
                    i += 2;
                }
                b'\\' => {
                    out.push(b'\\');
                    i += 2;
                }
                b'/' => {
                    out.push(b'/');
                    i += 2;
                }
                b'b' => {
                    out.push(0x08);
                    i += 2;
                }
                b'f' => {
                    out.push(0x0C);
                    i += 2;
                }
                b'n' => {
                    out.push(b'\n');
                    i += 2;
                }
                b'r' => {
                    out.push(b'\r');
                    i += 2;
                }
                b't' => {
                    out.push(b'\t');
                    i += 2;
                }
                b'u' if i + 5 < bytes.len() => {
                    if let Ok(hex_str) = std::str::from_utf8(&bytes[i + 2..i + 6])
                        && let Ok(hex) = u16::from_str_radix(hex_str, 16)
                    {
                        if (0xD800..=0xDBFF).contains(&hex)
                            && i + 11 < bytes.len()
                            && &bytes[i + 6..i + 8] == b"\\u"
                            && let Ok(low_str) = std::str::from_utf8(&bytes[i + 8..i + 12])
                            && let Ok(low) = u16::from_str_radix(low_str, 16)
                            && (0xDC00..=0xDFFF).contains(&low)
                        {
                            let cp =
                                0x10000 + (((hex as u32 - 0xD800) << 10) | (low as u32 - 0xDC00));
                            if let Some(ch) = char::from_u32(cp) {
                                let mut buf = [0u8; 4];
                                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                                i += 12;
                                continue;
                            }
                        }
                        if let Some(ch) = char::from_u32(hex as u32) {
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                            i += 6;
                            continue;
                        }
                    }
                    out.push(b'\\');
                    i += 1;
                }
                _ => {
                    out.push(b'\\');
                    i += 1;
                }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
}

pub(crate) fn find_delta_object(payload: &str) -> Option<usize> {
    let bytes = payload.as_bytes();
    let mut offset = 0;
    while let Some(rel) = memchr::memmem::find(&bytes[offset..], b"\"delta\"") {
        let idx = offset + rel;
        let mut i = idx + 7;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b':' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'{' {
                return Some(i + 1);
            }
        }
        offset = idx + 7;
    }
    None
}

/// Text fields borrowed from the first delta object, without decoding escapes.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DeltaFields<'a> {
    pub content: Option<&'a str>,
    pub reasoning_content: Option<&'a str>,
}

/// Locate a string's closing quote, skipping escaped quotes and backslashes.
fn string_end(bytes: &[u8], mut offset: usize) -> Option<usize> {
    while let Some(relative) = memchr::memchr2(b'"', b'\\', bytes.get(offset..)?) {
        offset += relative;
        if bytes[offset] == b'"' {
            return Some(offset);
        }
        offset += 2;
    }
    None
}

/// Read both text fields at delta depth one in one traversal. Nested tool-call
/// fields are ignored. Like the original field extractor, the first occurrence
/// wins even when it is null or non-string, and incomplete strings are ignored.
pub(crate) fn extract_delta_fields(payload: &str) -> DeltaFields<'_> {
    let bytes = payload.as_bytes();
    let Some(mut i) = find_delta_object(payload) else {
        return DeltaFields::default();
    };
    let mut fields = DeltaFields::default();
    let mut depth = 1usize;
    let mut seen_content = false;
    let mut seen_reasoning = false;
    while i < bytes.len() && depth > 0 {
        match bytes[i] {
            b'{' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                depth -= 1;
                i += 1;
            }
            b'"' => {
                let start = i + 1;
                let Some(end) = string_end(bytes, start) else {
                    break;
                };
                let key = &bytes[start..end];
                i = end + 1;
                let mut value = i;
                while bytes.get(value).is_some_and(u8::is_ascii_whitespace) {
                    value += 1;
                }
                if bytes.get(value) != Some(&b':') {
                    continue;
                }
                value += 1;
                while bytes.get(value).is_some_and(u8::is_ascii_whitespace) {
                    value += 1;
                }
                i = value;
                if depth != 1 {
                    continue;
                }
                let destination = match key {
                    b"content" if !seen_content => {
                        seen_content = true;
                        &mut fields.content
                    }
                    b"reasoning_content" if !seen_reasoning => {
                        seen_reasoning = true;
                        &mut fields.reasoning_content
                    }
                    _ => continue,
                };
                if bytes.get(value) == Some(&b'"') {
                    let Some(end) = string_end(bytes, value + 1) else {
                        break;
                    };
                    *destination = payload.get(value + 1..end);
                    i = end + 1;
                }
                if seen_content && seen_reasoning {
                    break;
                }
            }
            _ => i += 1,
        }
    }
    fields
}

/// `delta.content`, excluding fields nested in tool-call arguments.
pub(crate) fn extract_delta_content(payload: &str) -> Option<&str> {
    extract_delta_fields(payload).content
}

/// `delta.reasoning_content`, under the same delta boundary as content.
pub fn extract_reasoning_content(payload: &str) -> Option<&str> {
    extract_delta_fields(payload).reasoning_content
}

fn should_check_reasoning_fields(payload: &str) -> bool {
    if !payload.contains("reasoning") {
        return false;
    }
    (payload.contains("\"reasoning\":") && !payload.contains("\"reasoning_content\":"))
        || payload.contains("\"reasoning_details\":")
}

fn convert_reasoning_field(obj: &mut serde_json::Map<String, Value>) -> bool {
    let Some(reasoning) = obj.remove("reasoning") else {
        return false;
    };
    if let Some(text) = reasoning.as_str()
        && !text.is_empty()
        && !obj.contains_key("reasoning_content")
    {
        obj.insert(
            "reasoning_content".to_string(),
            serde_json::Value::String(text.to_string()),
        );
    }
    true
}

fn merge_reasoning_details(obj: &mut serde_json::Map<String, Value>, details: serde_json::Value) {
    let Some(arr) = details.as_array() else {
        return;
    };
    let combined: String = arr
        .iter()
        .filter_map(|d| d.get("text").and_then(|t| t.as_str()))
        .collect();
    if combined.is_empty() {
        return;
    }
    if let Some(serde_json::Value::String(existing_str)) = obj.get_mut("reasoning_content") {
        existing_str.push_str(&combined);
    } else {
        obj.insert(
            "reasoning_content".to_string(),
            serde_json::Value::String(combined),
        );
    }
}

fn apply_reasoning_normalizations(obj: &mut serde_json::Map<String, Value>) {
    let reasoning_was_present = convert_reasoning_field(obj);
    if let Some(details) = obj.remove("reasoning_details")
        && !reasoning_was_present
    {
        merge_reasoning_details(obj, details);
    }
}

pub fn normalize_nonstandard_reasoning_fields(payload: &str) -> Option<String> {
    if !should_check_reasoning_fields(payload) {
        return None;
    }

    let mut v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let choices = v.get_mut("choices")?.as_array_mut()?;
    let choice = choices.first_mut()?;
    let delta = choice.get_mut("delta")?;
    let obj = delta.as_object_mut()?;

    apply_reasoning_normalizations(obj);
    serde_json::to_string(&v).ok()
}

pub(crate) fn parse_tool_call_probe(payload: &str) -> Option<Vec<ToolCallProbe<'_>>> {
    if !payload.contains("\"tool_calls\"") {
        return None;
    }
    let v = serde_json::from_slice::<ToolCallProbeOuter<'_>>(payload.as_bytes()).ok()?;
    v.choices
        .and_then(|c| c.into_iter().next())
        .and_then(|c| c.delta)
        .and_then(|d| d.tool_calls)
}

pub(crate) fn extract_error_from_line(line: &[u8]) -> Option<(u16, String)> {
    let json_bytes = line
        .strip_prefix(b"data: ")
        .or_else(|| line.strip_prefix(b"data:"))
        .unwrap_or(line)
        .trim_ascii();
    if !json_bytes.starts_with(b"{") {
        return None;
    }
    let json_str = std::str::from_utf8(json_bytes).ok()?;
    let parsed = crate::sse::parse_inline_sse_error(json_str)?;
    Some((parsed.status_code, parsed.message.to_string()))
}
