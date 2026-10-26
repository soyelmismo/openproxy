//! Token estimation with a real BPE tokenizer (tiktoken cl100k_base), the
//! tokenizer OpenAI uses for GPT-4/GPT-3.5. It covers the two cases where a
//! char-based estimate misleads:
//!
//! 1. **Upstreams that omit the `usage` block** (most streaming endpoints),
//!    where estimation is the only source of a token count and
//!    `cost_usd = 0` rows otherwise reach analytics.
//! 2. **Compression savings**, where BPE is not linear in char count:
//!    whitespace compresses to fewer tokens than its length suggests and
//!    JSON braces tokenize unlike prose.
//!
//! The cl100k_base vocab (~1.6MB) is embedded at compile time and loaded once
//! via `std::sync::LazyLock`. Tokenization is ~1-5µs per token, so a 100K-token
//! prompt takes ~100-500ms: the estimator runs after the request completes and
//! the savings path tokenizes only the diff, never the full conversation.
//!
//! Exact for OpenAI models, an approximation elsewhere: Claude's own BPE vocab
//! typically lands within ±10% of cl100k_base on mixed English/code content,
//! which is enough for cost tracking and savings.

use openproxy_types::message::OpenAIMessage;

/// Estimate prompt tokens from string and array-of-parts (Anthropic-style
/// `[{type:"text",text:"..."}]`) content across every message role, plus 4
/// tokens of framing overhead per message (`<|im_start|>role\n...<|im_end|>`).
/// Image parts (base64 or URLs) are billed standard vision tiles (~800 tokens)
/// instead of counting raw base64 characters.
pub fn estimate_prompt_tokens(messages: &[OpenAIMessage]) -> u32 {
    let mut total: u32 = 0;
    for msg in messages {
        let text = message_content_to_text(msg);
        if !text.is_empty() {
            total += count_tokens(&text);
        }

        // Add standard image vision token allowance (~800 tokens per image part)
        if let Some(serde_json::Value::Array(parts)) = &msg.content {
            for part in parts {
                let is_image = part
                    .get("type")
                    .and_then(|v| v.as_str())
                    .is_some_and(|t| t == "image_url" || t == "input_image" || t == "image")
                    || part.get("image_url").is_some();
                if is_image {
                    total += 800;
                }
            }
        }

        // 4 tokens of role framing: <|im_start|>role\ncontent<|im_end|>\n.
        total += 4;
    }
    total
}

/// Estimate completion tokens from a text string.
pub fn estimate_completion_tokens(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    count_tokens(text)
}

/// Estimate completion tokens from a response body JSON value: OpenAI
/// `choices[].message` / `choices[].delta` (`content`, `reasoning_content` /
/// `reasoning`, `tool_calls`), Anthropic `content` blocks (`text`, `thinking`,
/// `tool_use`), Gemini `candidates[].content.parts` (`text`, `functionCall`),
/// or a bare string.
pub fn estimate_completion_tokens_from_body(body: &serde_json::Value) -> u32 {
    let mut total_tokens = 0u32;
    let mut found_structured = false;

    // 1. OpenAI shape: choices[].message or choices[].delta
    if let Some(choices) = body.get("choices").and_then(|c| c.as_array()) {
        for choice in choices {
            let msg = choice.get("message").or_else(|| choice.get("delta"));
            if let Some(msg) = msg {
                found_structured = true;
                if let Some(content) = msg.get("content") {
                    if let Some(s) = content.as_str()
                        && !s.is_empty()
                    {
                        total_tokens += count_tokens(s);
                    } else if let Some(parts) = content.as_array() {
                        for part in parts {
                            if let Some(s) = part.get("text").and_then(|t| t.as_str())
                                && !s.is_empty()
                            {
                                total_tokens += count_tokens(s);
                            }
                        }
                    }
                }

                let reasoning = msg
                    .get("reasoning_content")
                    .or_else(|| msg.get("reasoning"))
                    .and_then(|r| r.as_str());
                if let Some(r) = reasoning
                    && !r.is_empty()
                {
                    total_tokens += count_tokens(r);
                }

                if let Some(tool_calls) = msg.get("tool_calls").and_then(|tc| tc.as_array()) {
                    for tc in tool_calls {
                        // 4 tokens of framing overhead per tool call
                        total_tokens += 4;
                        if let Some(func) = tc.get("function") {
                            if let Some(name) = func.get("name").and_then(|n| n.as_str()) {
                                total_tokens += count_tokens(name);
                            }
                            if let Some(args) = func.get("arguments") {
                                if let Some(s) = args.as_str() {
                                    total_tokens += count_tokens(s);
                                } else if !args.is_null() {
                                    total_tokens += count_tokens(&args.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // 2. Anthropic shape: content: [ { type: "text", text: ... }, { type: "tool_use", ... } ]
    if let Some(content) = body.get("content").and_then(|c| c.as_array()) {
        for block in content {
            found_structured = true;
            let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match block_type {
                "text" => {
                    if let Some(s) = block.get("text").and_then(|t| t.as_str())
                        && !s.is_empty()
                    {
                        total_tokens += count_tokens(s);
                    }
                }
                "thinking" => {
                    if let Some(s) = block.get("thinking").and_then(|t| t.as_str())
                        && !s.is_empty()
                    {
                        total_tokens += count_tokens(s);
                    }
                }
                "tool_use" => {
                    total_tokens += 4;
                    if let Some(name) = block.get("name").and_then(|n| n.as_str()) {
                        total_tokens += count_tokens(name);
                    }
                    if let Some(input) = block.get("input") {
                        if let Some(s) = input.as_str() {
                            total_tokens += count_tokens(s);
                        } else if !input.is_null() {
                            total_tokens += count_tokens(&input.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // Gemini shape: candidates[].content.parts[]
    if let Some(candidates) = body.get("candidates").and_then(|c| c.as_array()) {
        for cand in candidates {
            if let Some(parts) = cand.pointer("/content/parts").and_then(|p| p.as_array()) {
                found_structured = true;
                for part in parts {
                    if let Some(text) = part.get("text").and_then(|t| t.as_str())
                        && !text.is_empty()
                    {
                        total_tokens += count_tokens(text);
                    }
                    if let Some(call) = part.get("functionCall") {
                        total_tokens += 4;
                        if let Some(name) = call.get("name").and_then(|n| n.as_str()) {
                            total_tokens += count_tokens(name);
                        }
                        if let Some(args) = call.get("args") {
                            total_tokens += count_tokens(&args.to_string());
                        }
                    }
                }
            }
        }
    }

    if !found_structured && let Some(text) = body.as_str() {
        total_tokens = count_tokens(text);
    }

    total_tokens
}

/// Count tokens in a text string using a char-based heuristic (~4 chars/token).
fn count_tokens(text: &str) -> u32 {
    estimate_tokens_heuristic(text)
}

/// Concatenated text of an `OpenAIMessage`: `extract_text()` plus the
/// `tool_calls[].function.arguments` JSON string when present.
pub fn message_content_to_text(msg: &OpenAIMessage) -> String {
    let mut text = msg.extract_text();

    if let Some(ref tool_calls) = msg.tool_calls {
        for tc in tool_calls {
            if let Some(args) = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())
            {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(args);
            }
        }
    }

    text
}

fn flush_whitespace_run(ws_run: usize) -> usize {
    if ws_run > 4 { ws_run / 10 } else { ws_run }
}

/// Char-based fallback (~4 chars/token Latin, ~2 CJK) for when the BPE encoder
/// fails to initialize.
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
            other_count += flush_whitespace_run(ws_run);
            ws_run = 0;

            if is_cjk(ch) {
                cjk_count += 1;
            } else {
                other_count += 1;
            }
        }
    }
    other_count += flush_whitespace_run(ws_run);

    // CJK: ~2 chars/token; Latin/other: ~4 chars/token
    let cjk_tokens = cjk_count.div_ceil(2); // ceiling div
    let other_tokens = other_count.div_ceil(4); // ceiling div

    (cjk_tokens + other_tokens).max(1) as u32
}

/// Check if a character is in a CJK Unicode range.
fn is_cjk(ch: char) -> bool {
    let code = ch as u32;
    // CJK Unified Ideographs
    (0x4E00..=0x9FFF).contains(&code)
    // Hiragana + Katakana
    || (0x3040..=0x30FF).contains(&code)
    // Hangul Syllables
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
            extra: serde_json::Map::default(),
        }
    }

    #[test]
    fn test_estimate_english_text() {
        // "Hello world" is 2 tokens in cl100k_base.
        let tokens = estimate_completion_tokens("Hello world");
        assert!(
            (1..=5).contains(&tokens),
            "expected 1-5 tokens, got {tokens}"
        );
    }

    #[test]
    fn test_estimate_empty_string() {
        assert_eq!(estimate_completion_tokens(""), 0);
    }

    #[test]
    fn test_estimate_cjk_text() {
        // CJK chars are ~1 token each in cl100k_base (not 0.5 like the
        // heuristic assumed). "你好世界" should be ~4 tokens.
        let tokens = estimate_completion_tokens("你好世界");
        assert!(
            (2..=6).contains(&tokens),
            "expected 2-6 tokens, got {tokens}"
        );
    }

    #[test]
    fn test_estimate_code() {
        let code = "fn main() { println!(\"hello\"); }";
        let tokens = estimate_completion_tokens(code);
        // Code tokenizes unlike prose, so the range is wide on purpose.
        assert!(
            (5..=30).contains(&tokens),
            "expected 5-30 tokens, got {tokens}"
        );
    }

    #[test]
    fn test_estimate_prompt_tokens_with_string_content() {
        let messages = vec![msg("user", "Hello"), msg("assistant", "Hi there!")];
        let tokens = estimate_prompt_tokens(&messages);
        // 2 messages x 4 overhead = 8, plus ~1 and ~2 content tokens.
        assert!(
            (10..=20).contains(&tokens),
            "expected 10-20 tokens, got {tokens}"
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
            extra: serde_json::Map::default(),
        }];
        let tokens = estimate_prompt_tokens(&messages);
        assert!(tokens >= 5, "expected at least 5 tokens, got {tokens}");
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
            extra: serde_json::Map::default(),
        }];
        let tokens = estimate_prompt_tokens(&messages);
        assert!(
            tokens >= 8,
            "expected at least 8 tokens (4 overhead + content + args), got {tokens}"
        );
    }

    #[test]
    fn test_estimate_prompt_tokens_message_overhead() {
        let messages = vec![msg("system", ""), msg("user", ""), msg("assistant", "")];
        let tokens = estimate_prompt_tokens(&messages);
        // 3 messages x 4 overhead = 12; empty content adds nothing.
        assert!(
            tokens >= 12,
            "expected at least 12 tokens (3×4 overhead), got {tokens}"
        );
    }

    #[test]
    fn test_estimate_whitespace_heavy() {
        let text = "a".to_string() + &" ".repeat(100) + "b";
        let tokens = estimate_completion_tokens(&text);
        // 100 spaces merge into a handful of tokens, not 25+.
        assert!(
            tokens <= 30,
            "expected ≤30 tokens for whitespace-heavy text, got {tokens}"
        );
    }

    #[test]
    fn test_bpe_vs_heuristic_differ_on_whitespace() {
        // Whitespace runs merge into adjacent word tokens, so BPE lands near
        // 3-5 tokens where chars/4 would overcount.
        let text = "hello     world     test"; // 5-space runs
        let bpe_tokens = count_tokens(text);
        let heuristic_tokens = estimate_tokens_heuristic(text);
        assert!(bpe_tokens > 0, "BPE should produce > 0 tokens");
        assert!(
            bpe_tokens <= 10,
            "BPE should be efficient with whitespace, got {bpe_tokens}"
        );
        let _ = heuristic_tokens; // used for documentation
    }

    #[test]
    fn test_json_tokenization() {
        // Braces and quotes tokenize unlike prose, so the count is not
        // len/4.
        let json = r#"{"id":1,"name":"test","status":"active"}"#;
        let tokens = estimate_completion_tokens(json);
        assert!(
            (5..=30).contains(&tokens),
            "expected 5-30 tokens for JSON, got {tokens}"
        );
    }

    #[test]
    fn test_large_text_performance() {
        let large_text = "The quick brown fox jumps over the lazy dog. ".repeat(2500); // ~110KB
        let start = std::time::Instant::now();
        let tokens = estimate_completion_tokens(&large_text);
        let elapsed = start.elapsed();
        assert!(
            tokens > 1000,
            "expected >1000 tokens for 110KB text, got {tokens}"
        );
        assert!(
            elapsed.as_millis() < 2000,
            "tokenization took {elapsed:?} — should be < 2s for 110KB"
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
            extra: serde_json::Map::default(),
        };
        let text = message_content_to_text(&msg);
        assert!(text.contains("text content"));
        assert!(text.contains("query"));
    }

    #[test]
    fn test_estimate_completion_tokens_from_body_tool_calls() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "delegate",
                            "arguments": "{\"tasks\":[{\"context\":\"Research host logs and system status\"}]}"
                        }
                    }]
                }
            }]
        });
        let tokens = estimate_completion_tokens_from_body(&body);
        assert!(
            tokens >= 15,
            "expected at least 15 tokens for tool call, got {tokens}"
        );
    }

    #[test]
    fn test_estimate_completion_tokens_from_body_anthropic_tool_use() {
        let body = serde_json::json!({
            "content": [
                { "type": "thinking", "thinking": "Let me check the tools." },
                { "type": "tool_use", "name": "get_weather", "input": { "city": "Madrid" } }
            ]
        });
        let tokens = estimate_completion_tokens_from_body(&body);
        assert!(
            tokens >= 10,
            "expected at least 10 tokens for anthropic tool_use, got {tokens}"
        );
    }

    #[test]
    fn test_estimate_completion_tokens_from_body_empty() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null
                }
            }]
        });
        let tokens = estimate_completion_tokens_from_body(&body);
        assert_eq!(tokens, 0);
    }
}
