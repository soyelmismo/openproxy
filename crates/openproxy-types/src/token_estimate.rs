//! Token estimation using a real BPE tokenizer (tiktoken cl100k_base).

use crate::message::OpenAIMessage;
use once_cell::sync::Lazy;
use tiktoken_rs::{CoreBPE, cl100k_base};

/// Thread-safe singleton BPE encoder. Loaded once at first use.
/// The cl100k_base vocab is ~50K tokens, embedded in the binary.
static ENCODER: Lazy<Option<CoreBPE>> = Lazy::new(|| match cl100k_base() {
    Ok(bpe) => {
        tracing::info!("tiktoken cl100k_base BPE encoder initialized");
        Some(bpe)
    }
    Err(e) => {
        tracing::error!(
            error = %e,
            "failed to initialize tiktoken cl100k_base encoder; token estimation will fall back to char heuristic"
        );
        None
    }
});

/// Estimate prompt tokens from a list of OpenAI messages using real BPE.
pub fn estimate_prompt_tokens(messages: &[OpenAIMessage]) -> u32 {
    let mut total: u32 = 0;
    for msg in messages {
        let text = message_content_to_text(msg);
        if !text.is_empty() {
            total += count_tokens(&text);
        }
        // Per-message overhead: 4 tokens for the role framing
        // (OpenAI's format: <|im_start|>role\ncontent<|im_end|>\n).
        total += 4;
    }
    total
}

/// Estimate completion tokens from a text string using real BPE.
pub fn estimate_completion_tokens(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    count_tokens(text)
}

/// Count tokens in a text string using cl100k_base BPE.
fn count_tokens(text: &str) -> u32 {
    if let Some(ref bpe) = *ENCODER {
        let tokens = bpe.encode_with_special_tokens(text);
        tokens.len() as u32
    } else {
        estimate_tokens_heuristic(text)
    }
}

/// Extract text from an OpenAIMessage's content field, handling both
/// string content and array-of-parts content (Anthropic-style).
pub fn message_content_to_text(msg: &OpenAIMessage) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Extract text from content field
    if let Some(ref content) = msg.content {
        if let Some(s) = content.as_str() {
            parts.push(s.to_string());
        } else if let Some(arr) = content.as_array() {
            for part in arr {
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    parts.push(text.to_string());
                }
            }
        }
    }

    // Extract tool_calls arguments (assistant messages with tool calls)
    if let Some(ref tool_calls) = msg.tool_calls {
        for tc in tool_calls {
            if let Some(args) = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())
            {
                parts.push(args.to_string());
            }
        }
    }

    parts.join("\n")
}

/// Char-based heuristic fallback (~4 chars/token for Latin, ~2 for CJK).
fn estimate_tokens_heuristic(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }

    let mut cjk_count: usize = 0;
    let mut other_count: usize = 0;
    let mut ws_run: usize = 0;

    for ch in text.chars() {
        if ch.is_whitespace() {
            ws_run += 1;
        } else {
            if ws_run > 4 {
                other_count += ws_run / 10;
            } else if ws_run > 0 {
                other_count += ws_run;
            }
            ws_run = 0;

            if is_cjk(ch) {
                cjk_count += 1;
            } else {
                other_count += 1;
            }
        }
    }
    // Handle trailing whitespace run
    if ws_run > 4 {
        other_count += ws_run / 10;
    } else if ws_run > 0 {
        other_count += ws_run;
    }

    // CJK: ~2 chars/token; Latin/other: ~4 chars/token
    let cjk_tokens = cjk_count.div_ceil(2); // ceiling div
    let other_tokens = other_count.div_ceil(4); // ceiling div

    (cjk_tokens + other_tokens).max(1) as u32
}

/// Check if a character is in a CJK Unicode range.
fn is_cjk(ch: char) -> bool {
    let code = ch as u32;
    (0x4E00..=0x9FFF).contains(&code)
        || (0x3040..=0x30FF).contains(&code)
        || (0xAC00..=0xD7AF).contains(&code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

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
    fn test_estimate_english_text() {
        let tokens = estimate_completion_tokens("Hello world");
        assert!(
            (1..=5).contains(&tokens),
            "expected 1-5 tokens, got {}",
            tokens
        );
    }

    #[test]
    fn test_estimate_empty_string() {
        assert_eq!(estimate_completion_tokens(""), 0);
    }

    #[test]
    fn test_estimate_cjk_text() {
        let tokens = estimate_completion_tokens("你好世界");
        assert!(
            (2..=6).contains(&tokens),
            "expected 2-6 tokens, got {}",
            tokens
        );
    }

    #[test]
    fn test_estimate_code() {
        let code = "fn main() { println!(\"hello\"); }";
        let tokens = estimate_completion_tokens(code);
        assert!(
            (5..=30).contains(&tokens),
            "expected 5-30 tokens, got {}",
            tokens
        );
    }

    #[test]
    fn test_estimate_prompt_tokens_with_string_content() {
        let messages = vec![msg("user", "Hello"), msg("assistant", "Hi there!")];
        let tokens = estimate_prompt_tokens(&messages);
        assert!(
            (10..=20).contains(&tokens),
            "expected 10-20 tokens, got {}",
            tokens
        );
    }

    #[test]
    fn test_estimate_prompt_tokens_with_array_content() {
        let messages = vec![OpenAIMessage {
            role: "user".to_string(),
            content: Some(serde_json::json!([
                {"type": "text", "text": "Hello from array"},
                {"type": "text", "text": "Second part"}
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        }];
        let tokens = estimate_prompt_tokens(&messages);
        assert!(tokens >= 5, "expected at least 5 tokens, got {}", tokens);
    }

    #[test]
    fn test_estimate_prompt_tokens_with_tool_calls() {
        let messages = vec![OpenAIMessage {
            role: "assistant".to_string(),
            content: Some(Value::String("Let me search".to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![serde_json::json!({
                "id": "call_1",
                "type": "function",
                "function": {
                    "name": "search",
                    "arguments": "{\"query\": \"hello world\"}"
                }
            })]),
            extra: Default::default(),
        }];
        let tokens = estimate_prompt_tokens(&messages);
        assert!(
            tokens >= 8,
            "expected at least 8 tokens (4 overhead + content + args), got {}",
            tokens
        );
    }

    #[test]
    fn test_estimate_prompt_tokens_message_overhead() {
        let messages = vec![msg("system", ""), msg("user", ""), msg("assistant", "")];
        let tokens = estimate_prompt_tokens(&messages);
        assert!(
            tokens >= 12,
            "expected at least 12 tokens (3×4 overhead), got {}",
            tokens
        );
    }

    #[test]
    fn test_estimate_whitespace_heavy() {
        let text = "a".to_string() + &" ".repeat(100) + "b";
        let tokens = estimate_completion_tokens(&text);
        assert!(
            tokens <= 30,
            "expected ≤30 tokens for whitespace-heavy text, got {}",
            tokens
        );
    }

    #[test]
    fn test_bpe_vs_heuristic_differ_on_whitespace() {
        let text = "hello     world     test";
        let bpe_tokens = count_tokens(text);
        assert!(bpe_tokens > 0, "BPE should produce > 0 tokens");
        assert!(
            bpe_tokens <= 10,
            "BPE should be efficient with whitespace, got {}",
            bpe_tokens
        );
    }

    #[test]
    fn test_json_tokenization() {
        let json = r#"{"id":1,"name":"test","status":"active"}"#;
        let tokens = estimate_completion_tokens(json);
        assert!(
            (5..=30).contains(&tokens),
            "expected 5-30 tokens for JSON, got {}",
            tokens
        );
    }

    #[test]
    fn test_large_text_performance() {
        let large_text = "The quick brown fox jumps over the lazy dog. ".repeat(2500);
        let start = std::time::Instant::now();
        let tokens = estimate_completion_tokens(&large_text);
        let elapsed = start.elapsed();
        assert!(
            tokens > 1000,
            "expected >1000 tokens for 110KB text, got {}",
            tokens
        );
        assert!(
            elapsed.as_millis() < 2000,
            "tokenization took {:?} — should be < 2s for 110KB",
            elapsed
        );
    }

    #[test]
    fn test_message_content_to_text_extracts_tool_args() {
        let msg = OpenAIMessage {
            role: "assistant".to_string(),
            content: Some(Value::String("text content".to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![serde_json::json!({
                "id": "call_1",
                "type": "function",
                "function": {
                    "name": "search",
                    "arguments": "{\"query\": \"test\"}"
                }
            })]),
            extra: Default::default(),
        };
        let text = message_content_to_text(&msg);
        assert!(text.contains("text content"));
        assert!(text.contains("query"));
    }
}
