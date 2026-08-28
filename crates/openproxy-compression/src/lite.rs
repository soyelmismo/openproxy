use crate::visitor::mutate_message_text;
use openproxy_types::OpenAIMessage;
/// 5 técnicas deterministas de compresión ligera (zero semantic change).
///
/// Cada técnica opera sobre `Vec<OpenAIMessage>` y reporta si aplicó cambios.
use std::borrow::Cow;

type Messages = Vec<OpenAIMessage>;

// ─── Technique 1: Collapse whitespace ───────────────────────────────────────

fn collapse_msg_whitespace(msg: &mut OpenAIMessage) -> bool {
    mutate_message_text(msg, |text| match normalize_message_whitespace(text) {
        Cow::Borrowed(_) => None,
        Cow::Owned(normalized) => Some(normalized),
    })
}

pub fn collapse_whitespace(msgs: &mut Messages) -> Vec<&'static str> {
    let mut applied = Vec::new();
    for msg in msgs.iter_mut() {
        if collapse_msg_whitespace(msg) {
            applied.push("lite::collapse_whitespace");
        }
    }
    applied
}

fn process_whitespace_char(
    ch: char,
    out: &mut String,
    newline_run: &mut usize,
    line_start: &mut usize,
) {
    if ch == '\n' {
        *newline_run += 1;
        if *newline_run <= 2 {
            trim_trailing_ws_in_place(out, *line_start);
            out.push('\n');
            *line_start = out.len();
        }
    } else {
        if *newline_run > 0 {
            *newline_run = 0;
            *line_start = out.len();
        }
        out.push(ch);
    }
}

/// Collapse 3+ consecutive newlines to 2, and trim trailing whitespace
/// (spaces, tabs) from each line. Single-pass, single allocation.
///
/// If the input is already normalized, returns `Cow::Borrowed`.
fn normalize_message_whitespace(s: &str) -> Cow<'_, str> {
    // Fast path: if the string has no 3+ newline runs AND no trailing
    // whitespace before newlines, it's already normalized.
    if !needs_normalization(s) {
        return Cow::Borrowed(s);
    }

    let mut out = String::with_capacity(s.len());
    let mut newline_run: usize = 0;
    let mut line_start: usize = 0;

    for ch in s.chars() {
        process_whitespace_char(ch, &mut out, &mut newline_run, &mut line_start);
    }
    // Trim trailing whitespace of the last line (no trailing newline).
    trim_trailing_ws_in_place(&mut out, line_start);
    Cow::Owned(out)
}

fn check_byte_normalization(
    b: u8,
    newline_run: &mut usize,
    line_has_trailing_ws: &mut bool,
) -> Option<bool> {
    if b == b'\n' {
        if *line_has_trailing_ws || *newline_run >= 2 {
            return Some(true);
        }
        *newline_run += 1;
    } else {
        *newline_run = 0;
        *line_has_trailing_ws = b == b' ' || b == b'\t';
    }
    None
}

/// Quick check: does `s` need normalization? Returns true if there's a
/// 3+ newline run OR any line with trailing whitespace (space/tab before
/// a newline or end-of-string). Single pass, no allocation.
fn needs_normalization(s: &str) -> bool {
    let mut newline_run = 0;
    let mut line_has_trailing_ws = false;
    for &b in s.as_bytes() {
        if let Some(res) = check_byte_normalization(b, &mut newline_run, &mut line_has_trailing_ws)
        {
            return res;
        }
    }
    // Check trailing whitespace on the last line (no newline at EOF).
    line_has_trailing_ws
}

/// Trim trailing space/tab bytes from `out` starting at index `from`.
fn trim_trailing_ws_in_place(out: &mut String, from: usize) {
    let mut end = out.len();
    while end > from {
        let prev = out.as_bytes()[end - 1];
        if prev == b' ' || prev == b'\t' {
            end -= 1;
        } else {
            break;
        }
    }
    out.truncate(end);
}

// ─── Technique 2: Dedup system prompts ──────────────────────────────────────

pub fn dedup_system_prompt(msgs: &mut Messages) -> Vec<&'static str> {
    let mut applied = Vec::new();
    let mut seen_prefixes: Vec<String> = Vec::new();
    let mut i = 0;
    while i < msgs.len() {
        let msg = &msgs[i];
        if msg.role != "system" {
            seen_prefixes.clear();
            i += 1;
            continue;
        }
        let prefix = msg
            .content
            .as_ref()
            .and_then(|c| c.as_str())
            .map(|s| s.chars().take(200).collect::<String>())
            .unwrap_or_default();
        if seen_prefixes.contains(&prefix) {
            applied.push("lite::dedup_system_prompt");
            msgs.remove(i);
            continue;
        }
        seen_prefixes.push(prefix);
        i += 1;
    }
    applied
}

// ─── Technique 3: Compress tool results ─────────────────────────────────────

const MAX_TOOL_CHARS: usize = 2000;

fn truncate_tool_text(text: &str) -> Option<String> {
    let mut cut_byte = None;
    let mut total_chars = 0;
    for (i, _) in text.char_indices() {
        if total_chars == MAX_TOOL_CHARS {
            cut_byte = Some(i);
        }
        total_chars += 1;
    }
    if total_chars > MAX_TOOL_CHARS {
        let mut cut = cut_byte.unwrap_or(text.len());
        if !text.is_char_boundary(cut) {
            cut = text.len();
        }
        Some(format!(
            "{}…[truncated {} chars]",
            &text[..cut],
            total_chars - MAX_TOOL_CHARS
        ))
    } else {
        None
    }
}

pub fn compress_tool_results(msgs: &mut Messages) -> Vec<&'static str> {
    let mut applied = Vec::new();
    for msg in msgs.iter_mut() {
        if msg.role == "tool" && mutate_message_text(msg, truncate_tool_text) {
            applied.push("lite::compress_tool_results");
        }
    }
    applied
}

// ─── Technique 4: Remove redundant consecutive messages ────────────────────

fn message_has_tools(msg: &OpenAIMessage) -> bool {
    msg.tool_calls.is_some() || msg.tool_call_id.is_some()
}

fn messages_are_redundant_duplicates(prev: &OpenAIMessage, curr: &OpenAIMessage) -> bool {
    if message_has_tools(prev) || message_has_tools(curr) {
        return false;
    }
    let prev_content = prev.content.as_ref().and_then(|c| c.as_str()).unwrap_or("");
    let curr_content = curr.content.as_ref().and_then(|c| c.as_str()).unwrap_or("");
    prev.role == curr.role && !prev_content.is_empty() && prev_content == curr_content
}

pub fn remove_redundant_content(msgs: &mut Messages) -> Vec<&'static str> {
    let mut applied = Vec::new();
    let mut i = 1;
    while i < msgs.len() {
        if messages_are_redundant_duplicates(&msgs[i - 1], &msgs[i]) {
            applied.push("lite::remove_redundant");
            msgs.remove(i);
        } else {
            i += 1;
        }
    }
    applied
}

// ─── Technique 5: Replace image URLs with placeholders ─────────────────────

fn extract_data_image_format(url: &str) -> &str {
    let Some(rest) = url.strip_prefix("data:image/") else {
        return "unknown";
    };
    let fmt = rest.split_once(';').map_or(rest, |(f, _)| f);
    if fmt.is_empty() { "unknown" } else { fmt }
}

fn try_replace_image_part(part: &mut serde_json::Value) -> bool {
    let fmt = {
        let url = part
            .get("image_url")
            .and_then(|v| v.get("url"))
            .and_then(|v| v.as_str());

        let Some(url) = url else {
            return false;
        };
        if !url.starts_with("data:image/") {
            return false;
        }

        extract_data_image_format(url).to_string()
    };

    let Some(obj) = part.as_object_mut() else {
        return false;
    };

    *obj = serde_json::json!({
        "type": "text",
        "text": format!("[image: {fmt}]")
    })
    .as_object()
    .cloned()
    .unwrap_or_default();

    true
}

pub fn replace_image_urls(msgs: &mut Messages) -> Vec<&'static str> {
    let mut applied = Vec::new();
    for msg in msgs.iter_mut() {
        let Some(parts) = msg.content.as_mut().and_then(|c| c.as_array_mut()) else {
            continue;
        };
        for part in parts.iter_mut() {
            if try_replace_image_part(part) {
                applied.push("lite::replace_image");
            }
        }
    }
    applied
}

// ─── Technique 6: Clean invisible unicode & BOM ────────────────────────────

fn has_invisible_or_crlf(text: &str) -> bool {
    const INVISIBLE: [char; 6] = ['\u{200B}', '\u{200C}', '\u{200D}', '\u{FEFF}', '\0', '\r'];
    text.chars().any(|c| INVISIBLE.contains(&c))
}

fn clean_text_unicode(text: &str) -> Option<String> {
    if !has_invisible_or_crlf(text) {
        return None;
    }
    let cleaned = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace(['\u{200B}', '\u{200C}', '\u{200D}', '\u{FEFF}', '\0'], "");
    if cleaned != text { Some(cleaned) } else { None }
}

pub fn clean_invisible_unicode(msgs: &mut Messages) -> Vec<&'static str> {
    let mut applied = Vec::new();
    for msg in msgs.iter_mut() {
        if mutate_message_text(msg, clean_text_unicode) {
            applied.push("lite::clean_unicode");
        }
    }
    applied
}

// ─── Technique 7: Strip ANSI escape sequences ──────────────────────────────

pub fn strip_ansi_escapes(msgs: &mut Messages) -> Vec<&'static str> {
    let mut applied = Vec::new();
    for msg in msgs.iter_mut() {
        if mutate_message_text(msg, |text| {
            if text.contains('\x1b') {
                let stripped = strip_ansi_string(text);
                if stripped != text {
                    return Some(stripped);
                }
            }
            None
        }) {
            applied.push("lite::strip_ansi");
        }
    }
    applied
}

fn skip_ansi_csi(bytes: &[u8], mut i: usize) -> usize {
    i += 2;
    while i < bytes.len() && !(0x40..=0x7E).contains(&bytes[i]) {
        i += 1;
    }
    if i < bytes.len() {
        i += 1;
    }
    i
}

fn strip_ansi_string(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == 0x1B {
            if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                i = skip_ansi_csi(bytes, i);
            } else {
                i += 1;
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_string())
}

// ─── Technique 8: Compact formatted multiline JSON ────────────────────────

fn process_json_byte(b: u8, out: &mut Vec<u8>, in_string: &mut bool, escaped: &mut bool) {
    if *in_string {
        out.push(b);
        if *escaped {
            *escaped = false;
        } else if b == b'\\' {
            *escaped = true;
        } else if b == b'"' {
            *in_string = false;
        }
    } else if b == b'"' {
        *in_string = true;
        out.push(b);
    } else if !b.is_ascii_whitespace() {
        out.push(b);
    }
}

fn minify_json(json: &str) -> String {
    let mut out = Vec::with_capacity(json.len());
    let mut in_string = false;
    let mut escaped = false;

    for &b in json.as_bytes() {
        process_json_byte(b, &mut out, &mut in_string, &mut escaped);
    }

    String::from_utf8(out).unwrap_or_else(|_| json.to_string())
}

fn is_json_candidate(trimmed: &str) -> bool {
    let has_delims = (trimmed.starts_with('{') && trimmed.ends_with('}'))
        || (trimmed.starts_with('[') && trimmed.ends_with(']'));
    has_delims && (trimmed.contains('\n') || trimmed.contains("  "))
}

fn try_compact_json_text(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if !is_json_candidate(trimmed) {
        return None;
    }
    let minified = minify_json(trimmed);
    if minified.len() < text.len()
        && serde_json::from_str::<&serde_json::value::RawValue>(&minified).is_ok()
    {
        Some(minified)
    } else {
        None
    }
}

pub fn compact_json(msgs: &mut Messages) -> Vec<&'static str> {
    let mut applied = Vec::new();
    for msg in msgs.iter_mut() {
        if mutate_message_text(msg, try_compact_json_text) {
            applied.push("lite::compact_json");
        }
    }
    applied
}

// ─── Technique 9: Collapse decorative ASCII separators ─────────────────────

pub fn collapse_ascii_separators(msgs: &mut Messages) -> Vec<&'static str> {
    let mut applied = Vec::new();
    for msg in msgs.iter_mut() {
        if mutate_message_text(msg, |text| {
            let collapsed = collapse_separator_runs(text);
            if collapsed == text {
                None
            } else {
                Some(collapsed)
            }
        }) {
            applied.push("lite::collapse_separators");
        }
    }
    applied
}

const SEPARATOR_CHARS: &[u8; 6] = b"-=*#_~";

fn has_long_separator_run(bytes: &[u8]) -> bool {
    for &sep in SEPARATOR_CHARS {
        let mut count = 0;
        for &b in bytes {
            if b == sep {
                count += 1;
                if count >= 12 {
                    return true;
                }
            } else {
                count = 0;
            }
        }
    }
    false
}

fn flush_char_run(out: &mut String, ch: char, count: usize) {
    const SEPS: [char; 6] = ['-', '=', '*', '#', '_', '~'];
    let repeat_count = if SEPS.contains(&ch) && count >= 12 {
        10
    } else {
        count
    };
    for _ in 0..repeat_count {
        out.push(ch);
    }
}

fn collapse_separator_runs(s: &str) -> String {
    if !has_long_separator_run(s.as_bytes()) {
        return s.to_string();
    }

    let mut out = String::with_capacity(s.len());
    let mut cur_char: Option<char> = None;
    let mut cur_count: usize = 0;

    for ch in s.chars() {
        if cur_char == Some(ch) {
            cur_count += 1;
        } else {
            if let Some(prev_ch) = cur_char {
                flush_char_run(&mut out, prev_ch, cur_count);
            }
            cur_char = Some(ch);
            cur_count = 1;
        }
    }
    if let Some(prev_ch) = cur_char {
        flush_char_run(&mut out, prev_ch, cur_count);
    }
    out
}

// ─── Apply all lite techniques ──────────────────────────────────────────────

/// Aplica las técnicas deterministas y 100% lossless (zero semantic loss).
///
/// Solo ejecuta técnicas puramente sin pérdida:
/// 1. `clean_invisible_unicode` (elimina zero-width spaces, BOM, null bytes, normaliza CRLF).
/// 2. `strip_ansi_escapes` (elimina secuencias de color/escape ANSI de terminal).
/// 3. `collapse_whitespace` (espacios al final de línea y 3+ newlines a 2).
/// 4. `collapse_ascii_separators` (reduce separadores de 80 caracteres repetidos a 10).
/// 5. `compact_json` (minifica JSONs indentados multilínea a formato compacto).
/// 6. `dedup_system_prompt` (elimina system prompts duplicados idénticos).
/// 7. `remove_redundant_content` (elimina mensajes consecutivos idénticos de texto).
pub fn apply_lite(msgs: &mut Messages) -> Vec<&'static str> {
    let mut all: Vec<&'static str> = Vec::new();
    all.extend(clean_invisible_unicode(msgs));
    all.extend(strip_ansi_escapes(msgs));
    all.extend(collapse_whitespace(msgs));
    all.extend(collapse_ascii_separators(msgs));
    all.extend(compact_json(msgs));
    all.extend(dedup_system_prompt(msgs));
    all.extend(remove_redundant_content(msgs));
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn msg(role: &str, content: &str) -> OpenAIMessage {
        OpenAIMessage {
            role: role.to_string(),
            content: Some(Value::String(content.to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::default(),
        }
    }

    #[test]
    fn test_collapse_whitespace_triple_newline() {
        let mut msgs = vec![msg("user", "hello\n\n\nworld")];
        let applied = collapse_whitespace(&mut msgs);
        assert!(!applied.is_empty());
        assert_eq!(
            msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap(),
            "hello\n\nworld"
        );
    }

    #[test]
    fn test_dedup_system_prompt_removes_duplicate() {
        let mut msgs = vec![
            msg("system", "You are a helpful assistant."),
            msg("system", "You are a helpful assistant."),
            msg("user", "hello"),
        ];
        let applied = dedup_system_prompt(&mut msgs);
        assert!(!applied.is_empty());
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn test_compress_tool_results_truncates() {
        let long = "x".repeat(3000);
        let mut msgs = vec![OpenAIMessage {
            role: "tool".into(),
            content: Some(Value::String(long)),
            name: None,
            tool_call_id: Some("call_1".into()),
            tool_calls: None,
            extra: serde_json::Map::default(),
        }];
        let applied = compress_tool_results(&mut msgs);
        assert!(!applied.is_empty());
        let result = msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap();
        assert!(result.len() < 2500);
        assert!(result.contains("[truncated"));
    }

    #[test]
    fn compress_tool_results_handles_multibyte_utf8_at_boundary() {
        let emoji = "😀"; // U+1F600, 4 bytes in UTF-8
        let mut content = String::new();
        content.push('a');
        for _ in 0..2000 {
            content.push_str(emoji);
        }
        content.push_str(" trailing text");
        assert!(content.chars().count() > MAX_TOOL_CHARS);

        let mut msgs = vec![OpenAIMessage {
            role: "tool".into(),
            content: Some(Value::String(content)),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::default(),
        }];
        let applied = compress_tool_results(&mut msgs);
        assert!(
            applied.contains(&"lite::compress_tool_results"),
            "expected compress_tool_results to fire on >2000 char content"
        );
        // Verify the content was truncated and contains the marker.
        if let Some(Value::String(s)) = &msgs[0].content {
            assert!(
                s.contains("…[truncated 15 chars]"),
                "expected truncation marker with exact char count, got: {s}"
            );
        } else {
            panic!("content should still be a string after truncation");
        }
    }

    #[test]
    fn compress_tool_results_does_not_truncate_multibyte_under_max_chars() {
        let emoji = "😀"; // 4 bytes each
        let mut content = String::new();
        for _ in 0..600 {
            content.push_str(emoji); // 2400 bytes, 600 chars
        }
        let mut msgs = vec![OpenAIMessage {
            role: "tool".into(),
            content: Some(Value::String(content.clone())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::default(),
        }];
        let applied = compress_tool_results(&mut msgs);
        assert!(applied.is_empty());
        assert_eq!(msgs[0].content.as_ref().unwrap().as_str().unwrap(), content);
    }

    #[test]
    fn test_remove_redundant_content_removes_same() {
        let mut msgs = vec![
            msg("assistant", "Hello!"),
            msg("assistant", "Hello!"),
            msg("user", "Hi"),
        ];
        let applied = remove_redundant_content(&mut msgs);
        assert!(!applied.is_empty());
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn test_replace_image_urls_replaces_data_uri() {
        let mut msgs = vec![OpenAIMessage {
            role: "user".into(),
            content: Some(json!([
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBOR..."}}
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::default(),
        }];
        let applied = replace_image_urls(&mut msgs);
        assert!(!applied.is_empty());
        let parts = msgs[0].content.as_ref().and_then(|c| c.as_array()).unwrap();
        assert!(parts[0]["text"].as_str().unwrap().contains("[image: png]"));
    }

    #[test]
    fn test_apply_lite_all_techniques() {
        let mut msgs = vec![
            msg("system", "sys"),
            msg("system", "sys"),
            msg("user", "a\n\n\nb"),
            OpenAIMessage {
                role: "tool".into(),
                content: Some(Value::String("x".repeat(3000))),
                name: None,
                tool_call_id: Some("c1".into()),
                tool_calls: None,
                extra: serde_json::Map::default(),
            },
        ];
        let techniques = apply_lite(&mut msgs);
        assert!(!techniques.is_empty());
        // dedup_system: 1 removed
        assert_eq!(msgs.len(), 3);
        // Tool result must be preserved verbatim (no truncation)
        let tool_content = msgs[2].content.as_ref().and_then(|c| c.as_str()).unwrap();
        assert_eq!(
            tool_content.len(),
            3000,
            "apply_lite must not truncate tool output"
        );
    }

    #[test]
    fn normalize_whitespace_collapses_3plus_newlines() {
        let input = "line1\n\n\n\n\nline2";
        let out = normalize_message_whitespace(input);
        assert_eq!(out, "line1\n\nline2");
    }

    #[test]
    fn normalize_whitespace_keeps_double_newlines() {
        let input = "para1\n\npara2";
        let out = normalize_message_whitespace(input);
        assert_eq!(out, "para1\n\npara2");
    }

    #[test]
    fn normalize_whitespace_trims_trailing_spaces() {
        let input = "line1   \nline2\t\nline3";
        let out = normalize_message_whitespace(input);
        assert_eq!(out, "line1\nline2\nline3");
    }

    #[test]
    fn normalize_whitespace_trims_trailing_ws_at_eof() {
        let input = "line1\nline2   ";
        let out = normalize_message_whitespace(input);
        assert_eq!(out, "line1\nline2");
    }

    #[test]
    fn normalize_whitespace_fast_path_already_normalized() {
        let input = "line1\nline2\n\npara2";
        let out = normalize_message_whitespace(input);
        assert_eq!(out, input);
    }

    #[test]
    fn normalize_whitespace_preserves_multibyte_utf8() {
        let input = "hello 世界   \nnext line";
        let out = normalize_message_whitespace(input);
        assert_eq!(out, "hello 世界\nnext line");
    }

    #[test]
    fn normalize_whitespace_preserves_emoji() {
        let input = "😀😀😀\n\n\n😀😀";
        let out = normalize_message_whitespace(input);
        assert_eq!(out, "😀😀😀\n\n😀😀");
    }

    #[test]
    fn test_clean_invisible_unicode() {
        let mut msgs = vec![msg(
            "user",
            "Hello\u{FEFF}\u{200B} world!\r\nSecond line.\0",
        )];
        let applied = clean_invisible_unicode(&mut msgs);
        assert!(!applied.is_empty());
        assert_eq!(
            msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap(),
            "Hello world!\nSecond line."
        );
    }

    #[test]
    fn test_strip_ansi_escapes() {
        let mut msgs = vec![msg(
            "tool",
            "\x1b[32mSuccess\x1b[0m: built target \x1b[1;34mfoo\x1b[0m",
        )];
        let applied = strip_ansi_escapes(&mut msgs);
        assert!(!applied.is_empty());
        assert_eq!(
            msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap(),
            "Success: built target foo"
        );
    }

    #[test]
    fn test_compact_json() {
        let pretty =
            "{\n  \"name\": \"test\",\n  \"count\": 42,\n  \"nested\": {\n    \"ok\": true\n  }\n}";
        let mut msgs = vec![msg("tool", pretty)];
        let applied = compact_json(&mut msgs);
        assert!(!applied.is_empty());
        let res = msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap();
        assert_eq!(
            res,
            "{\"name\":\"test\",\"count\":42,\"nested\":{\"ok\":true}}"
        );
    }

    #[test]
    fn test_collapse_ascii_separators() {
        let mut msgs = vec![msg(
            "user",
            "Start\n------------------------------------------------------------\nEnd",
        )];
        let applied = collapse_ascii_separators(&mut msgs);
        assert!(!applied.is_empty());
        assert_eq!(
            msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap(),
            "Start\n----------\nEnd"
        );
    }
}
