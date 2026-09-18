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

/// Extract a string field from `choices[0].delta` strictly at depth 1,
/// ignoring nested objects and arrays (e.g. `tool_calls`).
pub(crate) fn extract_delta_field<'a>(payload: &'a str, target_key: &str) -> Option<&'a str> {
    let bytes = payload.as_bytes();
    if !payload.contains(target_key) {
        return None;
    }
    let mut i = find_delta_object(payload)?;
    let mut depth: usize = 1;

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
                let str_start = i + 1;
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'"' {
                        break;
                    }
                    i += 1;
                }
                if i >= bytes.len() {
                    return None;
                }
                let key_str = std::str::from_utf8(&bytes[str_start..i]).ok()?;
                i += 1;

                let mut j = i;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b':' {
                    j += 1;
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if depth == 1 && key_str == target_key {
                        if j < bytes.len() && bytes[j] == b'"' {
                            let val_start = j + 1;
                            let mut k = val_start;
                            while k < bytes.len() {
                                if bytes[k] == b'\\' {
                                    k += 2;
                                    continue;
                                }
                                if bytes[k] == b'"' {
                                    return Some(&payload[val_start..k]);
                                }
                                k += 1;
                            }
                        }
                        return None;
                    }
                    i = j;
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    None
}

/// Extract `delta.content` from an OpenAI streaming chunk JSON payload
/// strictly within `choices[0].delta` object boundary, ignoring nested tool calls.
pub(crate) fn extract_delta_content(payload: &str) -> Option<&str> {
    extract_delta_field(payload, "content")
}

/// Extract `delta.reasoning_content` strictly within `choices[0].delta`.
pub fn extract_reasoning_content(payload: &str) -> Option<&str> {
    extract_delta_field(payload, "reasoning_content")
}

/// Normalize non-standard reasoning fields in an OpenAI streaming chunk.
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
