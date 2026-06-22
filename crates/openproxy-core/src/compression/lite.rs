/// 5 técnicas deterministas de compresión ligera (zero semantic change).
///
/// Cada técnica opera sobre `Vec<OpenAIMessage>` y reporta si aplicó cambios.
use crate::translation::OpenAIMessage;
use serde_json::Value;

type Messages = Vec<OpenAIMessage>;

// ─── Technique 1: Collapse whitespace ───────────────────────────────────────

pub fn collapse_whitespace(msgs: &mut Messages) -> Vec<&'static str> {
    let mut applied = Vec::new();
    for msg in msgs.iter_mut() {
        if let Some(ref mut content) = msg.content {
            if let Some(text) = content.as_str() {
                let normalized = normalize_message_whitespace(text);
                if normalized != text {
                    *content = Value::String(normalized);
                    applied.push("lite::collapse_whitespace");
                }
            } else if let Some(parts) = content.as_array_mut() {
                for part in parts.iter_mut() {
                    if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                        let normalized = normalize_message_whitespace(text);
                        if normalized != text {
                            part.as_object_mut()
                                .and_then(|o| o.insert("text".into(), Value::String(normalized)));
                            applied.push("lite::collapse_whitespace");
                        }
                    }
                }
            }
        }
    }
    applied
}

/// Collapse 3+ consecutive newlines to 2, and trim trailing whitespace
/// (spaces, tabs) from each line. Single-pass, single allocation.
///
/// If the input is already normalized, returns a clone (the caller's
/// `collapse_whitespace` wrapper checks for equality and skips the
/// write-back, so the clone is cheap insurance against a false-positive
/// "changed" detection).
fn normalize_message_whitespace(s: &str) -> String {
    // Fast path: if the string has no 3+ newline runs AND no trailing
    // whitespace before newlines, it's already normalized — clone and
    // return. This is the common case for well-formed prompts.
    if !needs_normalization(s) {
        return s.to_string();
    }

    let mut out = String::with_capacity(s.len());
    let mut newline_run: usize = 0;
    // Index in `out` where the current line starts (for trailing-ws trim).
    let mut line_start: usize = 0;

    for ch in s.chars() {
        if ch == '\n' {
            newline_run += 1;
            if newline_run <= 2 {
                // Trim trailing whitespace of the line we just finished.
                trim_trailing_ws_in_place(&mut out, line_start);
                out.push('\n');
                line_start = out.len();
            }
            // If newline_run > 2, we suppress the newline (collapse).
            continue;
        }
        if newline_run > 0 {
            // We were in a (suppressed or not) newline run; the next
            // non-newline char starts a fresh line.
            newline_run = 0;
            line_start = out.len();
        }
        out.push(ch);
    }
    // Trim trailing whitespace of the last line (no trailing newline).
    trim_trailing_ws_in_place(&mut out, line_start);
    out
}

/// Quick check: does `s` need normalization? Returns true if there's a
/// 3+ newline run OR any line with trailing whitespace (space/tab before
/// a newline or end-of-string). Single pass, no allocation.
fn needs_normalization(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut newline_run = 0;
    let mut line_has_trailing_ws = false;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\n' {
            if line_has_trailing_ws {
                return true;
            }
            newline_run += 1;
            if newline_run >= 3 {
                return true;
            }
        } else {
            if newline_run > 0 {
                newline_run = 0;
            }
            // Re-evaluate trailing-ws state based on the current byte
            // (always overwrites the previous value, so no need to clear
            // it in the newline-run branch above).
            line_has_trailing_ws = b == b' ' || b == b'\t';
        }
        i += 1;
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

pub fn compress_tool_results(msgs: &mut Messages) -> Vec<&'static str> {
    let mut applied = Vec::new();
    for msg in msgs.iter_mut() {
        if msg.role != "tool" {
            continue;
        }
        if let Some(ref mut content) = msg.content
            && let Some(text) = content.as_str()
            && text.len() > MAX_TOOL_CHARS
        {
            // Find the byte offset of the char at position MAX_TOOL_CHARS,
            // so we don't slice in the middle of a multi-byte UTF-8 sequence.
            let cut = text
                .char_indices()
                .nth(MAX_TOOL_CHARS)
                .map(|(i, _)| i)
                .unwrap_or(text.len());
            let truncated = format!(
                "{}…[truncated {} chars]",
                &text[..cut],
                text.len() - cut
            );
            *content = Value::String(truncated);
            applied.push("lite::compress_tool_results");
        }
    }
    applied
}

// ─── Technique 4: Remove redundant consecutive messages ────────────────────

pub fn remove_redundant_content(msgs: &mut Messages) -> Vec<&'static str> {
    let mut applied = Vec::new();
    let mut i = 1;
    while i < msgs.len() {
        let prev = &msgs[i - 1];
        let curr = &msgs[i];
        let prev_content = prev.content.as_ref().and_then(|c| c.as_str()).unwrap_or("");
        let curr_content = curr.content.as_ref().and_then(|c| c.as_str()).unwrap_or("");
        if prev.role == curr.role && !prev_content.is_empty() && prev_content == curr_content {
            applied.push("lite::remove_redundant");
            msgs.remove(i);
            continue;
        }
        i += 1;
    }
    applied
}

// ─── Technique 5: Replace image URLs with placeholders ─────────────────────

pub fn replace_image_urls(msgs: &mut Messages) -> Vec<&'static str> {
    let mut applied = Vec::new();
    for msg in msgs.iter_mut() {
        if let Some(ref mut content) = msg.content
            && let Some(parts) = content.as_array_mut()
        {
            for part in parts.iter_mut() {
                let is_data_image = part
                    .get("image_url")
                    .and_then(|v| v.get("url"))
                    .and_then(|v| v.as_str())
                    .map(|url| url.starts_with("data:image/"))
                    .unwrap_or(false);
                if !is_data_image {
                    continue;
                }
                let fmt = part
                    .get("image_url")
                    .and_then(|v| v.get("url"))
                    .and_then(|v| v.as_str())
                    .map(|url| {
                        let semi = url.find(';').unwrap_or(url.len());
                        let fmt = &url["data:image/".len()..semi];
                        if fmt.is_empty() { "unknown".to_string() } else { fmt.to_string() }
                    })
                    .unwrap_or_else(|| "unknown".to_string());
                if let Some(obj) = part.as_object_mut() {
                    *obj = serde_json::json!({
                        "type": "text",
                        "text": format!("[image: {}]", fmt)
                    })
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
                    applied.push("lite::replace_image");
                }
            }
        }
    }
    applied
}

// ─── Apply all lite techniques ──────────────────────────────────────────────

/// Aplica las 5 técnicas lite secuencialmente. Retorna las técnicas que aplicaron.
pub fn apply_lite(msgs: &mut Messages) -> Vec<&'static str> {
    let mut all: Vec<&'static str> = Vec::new();
    all.extend(collapse_whitespace(msgs));
    all.extend(dedup_system_prompt(msgs));
    all.extend(compress_tool_results(msgs));
    all.extend(remove_redundant_content(msgs));
    all.extend(replace_image_urls(msgs));
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(role: &str, content: &str) -> OpenAIMessage {
        OpenAIMessage {
            role: role.to_string(),
            content: Some(Value::String(content.to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
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
        let mut msgs = vec![
            OpenAIMessage {
                role: "tool".into(),
                content: Some(Value::String(long)),
                name: None,
                tool_call_id: Some("call_1".into()),
                tool_calls: None,
                extra: Default::default(),
            },
        ];
        let applied = compress_tool_results(&mut msgs);
        assert!(!applied.is_empty());
        let result = msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap();
        assert!(result.len() < 2500);
        assert!(result.contains("[truncated"));
    }

    #[test]
    fn compress_tool_results_handles_multibyte_utf8_at_boundary() {
        // Regression test for a UTF-8 panic in compress_tool_results.
        //
        // Pre-fix the code did `&text[..MAX_TOOL_CHARS]` (i.e. `&text[..2000]`),
        // which panics with "byte index 2000 is not a char boundary" when byte
        // 2000 lands in the middle of a multi-byte UTF-8 sequence.
        //
        // Construction: 1 ASCII byte + 500 emojis (4 bytes each) = 2001 bytes.
        // Emoji #500 occupies bytes 1997..=2000, so byte index 2000 is the LAST
        // byte of that emoji — mid-char, NOT a boundary. Slicing at 2000 would
        // panic. (Naively using 501 emojis does NOT trigger the bug because byte
        // 2000 then lands on the start of emoji #501, which is a boundary.)
        let emoji = "😀"; // U+1F600, 4 bytes in UTF-8
        let mut content = String::new();
        content.push('a');
        for _ in 0..500 {
            content.push_str(emoji);
        }
        content.push_str(" trailing text");
        assert!(content.len() > MAX_TOOL_CHARS);

        let mut msgs = vec![OpenAIMessage {
            role: "tool".into(),
            content: Some(Value::String(content)),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        }];
        let applied = compress_tool_results(&mut msgs);
        assert!(
            applied.contains(&"lite::compress_tool_results"),
            "expected compress_tool_results to fire on >2000 byte content"
        );
        // Verify the content was truncated and contains the marker.
        if let Some(Value::String(s)) = &msgs[0].content {
            assert!(
                s.contains("…[truncated"),
                "expected truncation marker, got: {}",
                s
            );
        } else {
            panic!("content should still be a string after truncation");
        }
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
        let mut msgs = vec![
            OpenAIMessage {
                role: "user".into(),
                content: Some(json!([
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBOR..."}}
                ])),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: Default::default(),
            },
        ];
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
                extra: Default::default(),
            },
        ];
        let techniques = apply_lite(&mut msgs);
        assert!(!techniques.is_empty());
        // dedup_system: 1 removed
        assert_eq!(msgs.len(), 3);
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
}
