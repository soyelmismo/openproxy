//! Token estimation for requests where the upstream doesn't return usage data.
//!
//! Uses a heuristic approach (~4 chars/token for Latin text, ~2 chars/token
//! for CJK) since adding a real BPE tokenizer (tiktoken, ~3MB vocab) would
//! bloat the binary. The estimate is accurate enough for cost tracking and
//! compression savings calculation — it's typically within ±20% of the real
//! token count for mixed English/code/JSON content.

use crate::translation::OpenAIMessage;

/// Estimate prompt tokens from a list of OpenAI messages.
///
/// This walks every message (system, user, assistant, tool), extracts text
/// from both string content and array-of-parts content (Anthropic-style
/// `[{type:"text",text:"..."}]`), and applies the heuristic.
///
/// Includes a per-message overhead of 4 tokens (matching OpenAI's documented
/// "every message follows this format: <|start|>role<|msg|>content<|end|>"
/// overhead, which is ~4 tokens per message in GPT-4's tokenizer).
pub fn estimate_prompt_tokens(messages: &[OpenAIMessage]) -> u32 {
    let mut total: u32 = 0;
    for msg in messages {
        let text = message_content_to_text(msg);
        // Per-message overhead — applied unconditionally so empty
        // assistant messages (e.g. one that carried only `tool_calls`)
        // still cost their structural tokens.
        total += 4;
        total += estimate_tokens_from_text(&text);
    }
    total
}

/// Estimate completion tokens from a text string.
pub fn estimate_completion_tokens(text: &str) -> u32 {
    estimate_tokens_from_text(text)
}

/// Extract text from an OpenAIMessage's content field, handling both
/// string content and array-of-parts content (Anthropic-style).
/// Returns the concatenated text.
///
/// Also folds in `tool_calls[].function.arguments` (a JSON string) for
/// assistant messages that carry tool calls but no textual content —
/// otherwise those messages would be billed at zero tokens despite
/// carrying meaningful prompt payload.
pub fn message_content_to_text(msg: &OpenAIMessage) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(content) = &msg.content {
        match content {
            serde_json::Value::String(s) => {
                if !s.is_empty() {
                    parts.push(s.clone());
                }
            }
            serde_json::Value::Array(arr) => {
                for part in arr {
                    // Anthropic-style: [{type:"text", text:"..."}].
                    // We don't pull `image` / `tool_use` blocks here —
                    // they're not text and would skew the char count.
                    if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                        if !text.is_empty() {
                            parts.push(text.to_string());
                        }
                    }
                }
            }
            // `null` content (assistant messages with only tool_calls) →
            // empty string, NOT "null" which would bill 1 spurious token.
            _ => {}
        }
    }

    // Assistant tool_calls carry the function arguments as a JSON string.
    // Include them so the prompt token count reflects what was actually
    // sent upstream.
    if let Some(tool_calls) = &msg.tool_calls {
        for tc in tool_calls {
            if let Some(function) = tc.get("function") {
                if let Some(args) = function.get("arguments").and_then(|v| v.as_str()) {
                    if !args.is_empty() {
                        parts.push(args.to_string());
                    }
                }
            }
        }
    }

    parts.join("\n")
}

/// Core heuristic: estimate tokens from a text string.
/// - CJK characters (Chinese, Japanese, Korean) ≈ 2 chars/token
/// - Latin/ASCII characters ≈ 4 chars/token
/// - Whitespace runs count as fewer tokens (compressed)
/// - Minimum 1 token for any non-empty string
fn estimate_tokens_from_text(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }

    let mut tokens: u32 = 0;
    let mut cjk_count: usize = 0;
    let mut other_count: usize = 0;
    let mut ws_run: usize = 0;

    for c in text.chars() {
        if c.is_whitespace() {
            ws_run += 1;
            continue;
        }
        // Flush the pending whitespace run before resuming
        // text accumulation. Short runs (<=4) are folded into
        // `other_count` so a single space between words bills
        // at the regular 4 chars/token rate rather than getting
        // its own token — this matches how "Hello world" yields
        // ~3 tokens rather than 4.
        if ws_run > 0 {
            if ws_run > 4 {
                // Long whitespace runs tokenize efficiently (consecutive
                // spaces collapse in BPE): ~1 token per 10 chars, rounded up.
                tokens += ((ws_run + 9) / 10) as u32;
            } else {
                other_count += ws_run;
            }
            ws_run = 0;
        }
        if is_cjk(c) {
            cjk_count += 1;
        } else {
            other_count += 1;
        }
    }
    // Flush a trailing whitespace run.
    if ws_run > 0 {
        if ws_run > 4 {
            tokens += ((ws_run + 9) / 10) as u32;
        } else {
            other_count += ws_run;
        }
    }

    // Ceiling division — tokenizers round up, and over-estimating by
    // a fraction of a token is preferable to under-estimating for
    // cost tracking.
    tokens += ((cjk_count + 1) / 2) as u32;
    tokens += ((other_count + 3) / 4) as u32;

    // Minimum 1 token for any non-empty string (already guaranteed
    // non-empty at the top of the function, but be defensive against
    // a string of only whitespace that mathematically rounds to 0).
    tokens.max(1)
}

/// Count CJK characters in a string (for adjusting the heuristic).
#[allow(dead_code)]
fn count_cjk_chars(text: &str) -> usize {
    text.chars().filter(|c| is_cjk(*c)).count()
}

/// Is this character in a CJK Unicode block?
///
/// - U+4E00..U+9FFF : CJK Unified Ideographs (Chinese Hanzi, Japanese Kanji)
/// - U+3040..U+30FF : Hiragana + Katakana (Japanese)
/// - U+AC00..U+D7AF : Hangul Syllables (Korean)
///
/// (Not exhaustive — CJK Extension A/B, Bopomofo, etc. are rare in
/// prompts and omitting them keeps the check a single `matches!`.)
fn is_cjk(c: char) -> bool {
    matches!(
        c,
        '\u{4E00}'..='\u{9FFF}'
            | '\u{3040}'..='\u{30FF}'
            | '\u{AC00}'..='\u{D7AF}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translation::OpenAIMessage;
    use serde_json::json;

    fn msg(role: &str, content: serde_json::Value) -> OpenAIMessage {
        OpenAIMessage {
            role: role.to_string(),
            content: Some(content),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn test_estimate_english_text() {
        // "Hello world" — 11 chars, single space. Expected ~3 tokens.
        let tokens = estimate_tokens_from_text("Hello world");
        assert_eq!(tokens, 3);
    }

    #[test]
    fn test_estimate_empty_string() {
        assert_eq!(estimate_tokens_from_text(""), 0);
        assert_eq!(estimate_completion_tokens(""), 0);
    }

    #[test]
    fn test_estimate_cjk_text() {
        // "你好世界" — 4 CJK chars. ~2 tokens at 2 chars/token.
        let tokens = estimate_tokens_from_text("你好世界");
        assert_eq!(tokens, 2);
        assert_eq!(count_cjk_chars("你好世界"), 4);
    }

    #[test]
    fn test_estimate_mixed_content() {
        // "Hello 你好 world" — 5 latin + 1 ws + 2 CJK + 1 ws + 5 latin = 14 chars
        // other_count = 5 + 1 + 1 + 5 = 12 → ceil(12/4) = 3
        // cjk_count    = 2              → ceil(2/2) = 1
        // total = 4
        let tokens = estimate_tokens_from_text("Hello 你好 world");
        assert_eq!(tokens, 4);
    }

    #[test]
    fn test_estimate_code() {
        // Rust snippet — should produce a reasonable non-zero estimate
        // without over-counting. No CJK, mostly ASCII.
        let code = r#"fn main() {
    println!("hello");
}"#;
        let tokens = estimate_tokens_from_text(code);
        // 30-ish chars → ~8-12 tokens. Just sanity check it's reasonable.
        assert!(tokens >= 6 && tokens <= 15, "got {tokens}");
    }

    #[test]
    fn test_estimate_prompt_tokens_with_string_content() {
        // Two messages: "Hello" + "world"
        // "Hello" → ceil(5/4) = 2
        // "world" → ceil(5/4) = 2
        // + 4 overhead each = 8
        // total = 12
        let messages = vec![
            msg("user", json!("Hello")),
            msg("assistant", json!("world")),
        ];
        let tokens = estimate_prompt_tokens(&messages);
        assert_eq!(tokens, 12);
    }

    #[test]
    fn test_estimate_prompt_tokens_with_array_content() {
        // Anthropic-style array-of-parts content.
        // Single part "Hello world" → 3 tokens, + 4 overhead = 7
        let messages = vec![msg(
            "user",
            json!([{"type": "text", "text": "Hello world"}]),
        )];
        let tokens = estimate_prompt_tokens(&messages);
        assert_eq!(tokens, 7);
    }

    #[test]
    fn test_estimate_prompt_tokens_with_tool_calls() {
        // Assistant message with null content + tool_calls.
        // arguments = '{"location":"Boston"}' (21 chars) → ceil(21/4) = 6
        // + 4 overhead = 10
        let mut m = msg("assistant", serde_json::Value::Null);
        m.tool_calls = Some(vec![json!({
            "id": "call_abc",
            "type": "function",
            "function": {
                "name": "get_weather",
                "arguments": "{\"location\":\"Boston\"}"
            }
        })]);
        let tokens = estimate_prompt_tokens(&[m]);
        // 21 args chars → 6 tokens + 4 overhead = 10
        assert_eq!(tokens, 10);
    }

    #[test]
    fn test_estimate_prompt_tokens_message_overhead() {
        // Three empty messages → 3 * 4 = 12 tokens of pure overhead.
        let messages = vec![
            msg("system", serde_json::Value::Null),
            msg("user", serde_json::Value::Null),
            msg("assistant", serde_json::Value::Null),
        ];
        let tokens = estimate_prompt_tokens(&messages);
        assert_eq!(tokens, 12);
    }

    #[test]
    fn test_estimate_whitespace_heavy() {
        // 100 spaces → ceil(100/10) = 10 tokens (not 25).
        let tokens = estimate_tokens_from_text(&" ".repeat(100));
        assert_eq!(tokens, 10);

        // Single long whitespace run folded into surrounding text.
        // "a" + 50 spaces + "b" → other=2, ws_run=50 → 5 tokens
        // total = ceil(2/4) + 5 = 1 + 5 = 6
        let text = format!("a{}b", " ".repeat(50));
        let tokens = estimate_tokens_from_text(&text);
        assert_eq!(tokens, 6);

        // A 100-space string shouldn't bill more than a similar-length
        // text string — sanity check the compression benefit.
        let text_tokens = estimate_tokens_from_text(&"a".repeat(100));
        assert!(tokens < text_tokens);
    }
}
