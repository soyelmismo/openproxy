//! Compression pipeline: Lite + RTK modes.
//!
//! # Modes
//! - `Off`: No compression, zero overhead.
//! - `Lite`: 5 deterministic text-normalization techniques + content-shape
//!   routing (SmartCrusher for JSON arrays, LogCompressor for build logs,
//!   DiffCompressor for git diffs). Zero semantic change for text; lossless-first
//!   for JSON; lossy-but-recoverable for logs/diffs.
//! - `Rtk`: Command-aware filtering for CLI tool output (git, test, build, etc.).
//! - `LiteRtk`: Both Lite and Rtk, in that order.

pub mod content_router;
pub mod diff_compressor;
pub mod lite;
pub mod log_compressor;
pub mod rtk;
pub mod smart_crusher;
pub mod stats;

use crate::translation::OpenAIMessage;
use serde::{Deserialize, Serialize};
use stats::CompressionStats;

/// Modo de compresión.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompressionMode {
    #[default]
    Off,
    Lite,
    Rtk,
    LiteRtk,
}

/// Aplica compresión a los mensajes del request según el modo.
///
/// Modifica `messages` in-place y retorna estadísticas de la compresión.
pub fn apply_compression(
    messages: &mut Vec<OpenAIMessage>,
    mode: CompressionMode,
) -> CompressionStats {
    match mode {
        CompressionMode::Off => CompressionStats::empty(),
        CompressionMode::Lite => {
            let original_chars = count_content_chars(messages);
            let original_tokens = crate::token_estimate::estimate_prompt_tokens(messages) as usize;
            // Content routing runs FIRST so SmartCrusher/LogCompressor/
            // DiffCompressor see the full content before lite's brute
            // truncation (compress_tool_results) kicks in as a fallback.
            let mut techniques = apply_content_routing(messages);
            techniques.extend(
                lite::apply_lite(messages)
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect::<Vec<_>>(),
            );
            let compressed_chars = count_content_chars(messages);
            let compressed_tokens = crate::token_estimate::estimate_prompt_tokens(messages) as usize;
            CompressionStats::new(
                original_chars,
                compressed_chars,
                original_tokens,
                compressed_tokens,
                techniques,
            )
        }
        CompressionMode::Rtk => {
            let original_chars = count_content_chars(messages);
            let original_tokens = crate::token_estimate::estimate_prompt_tokens(messages) as usize;
            let techniques = rtk::apply_rtk(messages);
            let compressed_chars = count_content_chars(messages);
            let compressed_tokens = crate::token_estimate::estimate_prompt_tokens(messages) as usize;
            CompressionStats::new(
                original_chars,
                compressed_chars,
                original_tokens,
                compressed_tokens,
                techniques,
            )
        }
        CompressionMode::LiteRtk => {
            let original_chars = count_content_chars(messages);
            let original_tokens = crate::token_estimate::estimate_prompt_tokens(messages) as usize;
            // Content routing first (smart compression), then lite
            // (normalization + fallback truncation), then rtk
            // (command-aware CLI filtering).
            let mut techniques = apply_content_routing(messages);
            techniques.extend(
                lite::apply_lite(messages)
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect::<Vec<_>>(),
            );
            techniques.extend(rtk::apply_rtk(messages));
            let compressed_chars = count_content_chars(messages);
            let compressed_tokens = crate::token_estimate::estimate_prompt_tokens(messages) as usize;
            CompressionStats::new(
                original_chars,
                compressed_chars,
                original_tokens,
                compressed_tokens,
                techniques,
            )
        }
    }
}

/// Content-shape routing: for each tool/assistant message, detect the
/// content type and dispatch to the appropriate compressor (SmartCrusher
/// for JSON arrays, LogCompressor for build logs, DiffCompressor for
/// git diffs). Runs BEFORE lite's basic normalization so the smart
/// compressors see the full content. Lite's `compress_tool_results`
/// truncation then acts as a fallback for anything the router couldn't
/// handle (or that's still too large after smart compression).
///
/// This is called by `apply_compression` for both `Lite` and `LiteRtk`
/// modes. It operates on `role == "tool"` and `role == "assistant"`
/// messages whose content is a plain string (not array-of-parts).
fn apply_content_routing(messages: &mut Vec<OpenAIMessage>) -> Vec<String> {
    let mut techniques: Vec<String> = Vec::new();
    for msg in messages.iter_mut() {
        // Only route tool results and assistant messages — user/system
        // messages are the operator's intent and must not be compressed
        // by the content router.
        if msg.role != "tool" && msg.role != "assistant" {
            continue;
        }
        // Only route string content (not array-of-parts with images/etc).
        let content_str = match msg.content.as_ref().and_then(|c| c.as_str()) {
            Some(s) => s,
            None => continue,
        };
        // Skip tiny content — not worth the detection overhead.
        if content_str.len() < 500 {
            continue;
        }
        // Route to the appropriate compressor based on content shape.
        if let Some((compressed, technique)) = content_router::route_content(content_str) {
            // Safety: only apply if the compressor actually reduced the size.
            if compressed.len() < content_str.len() {
                msg.content = Some(serde_json::Value::String(compressed));
                techniques.push(technique.to_string());
            }
        }
    }
    techniques
}

/// Cuenta chars totales del contenido textual de los mensajes.
fn count_content_chars(msgs: &[OpenAIMessage]) -> usize {
    msgs.iter()
        .filter_map(|m| m.content.as_ref())
        .filter_map(|c| c.as_str())
        .map(|s| s.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translation::OpenAIMessage;
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
    fn test_lite_rtk_applies_both() {
        // Message 1: triple+ newline triggers lite::collapse_whitespace.
        // Message 2: "git status" output triggers the rtk git-status filter.
        let mut messages = vec![
            msg("user", "hello\n\n\n\nworld"),
            msg(
                "user",
                "$ git status\nOn branch main\n  (use \"git add\" to update)\n\tmodified: foo.rs\nnothing added to commit\n",
            ),
        ];
        let stats = apply_compression(&mut messages, CompressionMode::LiteRtk);

        let techniques = stats.techniques;
        assert!(
            techniques.iter().any(|t| t.starts_with("lite::")),
            "expected at least one lite:: technique, got: {:?}",
            techniques
        );
        // rtk rules are formatted as "{filter_id}::{rule}" where filter_id is
        // e.g. "git-status" or "generic". Distinguish them from lite rules
        // by requiring a non-lite prefix.
        assert!(
            techniques.iter().any(|t| !t.starts_with("lite::")),
            "expected at least one non-lite (rtk-derived) technique, got: {:?}",
            techniques
        );
        assert!(stats.compressed_chars <= stats.original_chars);
    }

    #[test]
    fn test_lite_routes_json_array_to_smart_crusher() {
        // A tool result containing a JSON array of 20 homogeneous items
        // should trigger SmartCrusher via the content router.
        let mut array = Vec::new();
        for i in 0..20 {
            array.push(serde_json::json!({
                "id": i,
                "name": format!("item{}", i),
                "status": "active",
                "value": i * 10,
            }));
        }
        let json_content = serde_json::to_string(&array).unwrap();
        let original_len = json_content.len();
        let mut messages = vec![OpenAIMessage {
            role: "tool".into(),
            content: Some(Value::String(json_content)),
            name: None,
            tool_call_id: Some("call_1".into()),
            tool_calls: None,
            extra: Default::default(),
        }];
        let stats = apply_compression(&mut messages, CompressionMode::Lite);
        assert!(
            stats.techniques.iter().any(|t| t == "lite::smart_crusher_lossless"),
            "expected smart_crusher_lossless technique, got: {:?}",
            stats.techniques
        );
        let compressed = messages[0].content.as_ref().and_then(|c| c.as_str()).unwrap();
        assert!(
            compressed.len() < original_len,
            "compressed ({}) should be smaller than original ({})",
            compressed.len(),
            original_len
        );
        // CSV schema marker should be present
        assert!(compressed.contains("#schema:"), "expected CSV schema header");
    }

    #[test]
    fn test_lite_routes_git_diff_to_diff_compressor() {
        // A tool result containing a 40-line git diff should trigger
        // DiffCompressor via the content router.
        let mut diff = String::from("diff --git a/foo.rs b/foo.rs\n");
        diff.push_str("index abc..def 100644\n");
        diff.push_str("--- a/foo.rs\n");
        diff.push_str("+++ b/foo.rs\n");
        diff.push_str("@@ -1,40 +1,40 @@\n");
        for i in 0..40 {
            diff.push_str(&format!(" context line {}\n", i));
        }
        // Add some actual changes
        diff.push_str("+new line 1\n");
        diff.push_str("+new line 2\n");
        diff.push_str("-old line 1\n");
        let original_len = diff.len();
        let mut messages = vec![OpenAIMessage {
            role: "tool".into(),
            content: Some(Value::String(diff)),
            name: None,
            tool_call_id: Some("call_1".into()),
            tool_calls: None,
            extra: Default::default(),
        }];
        let stats = apply_compression(&mut messages, CompressionMode::Lite);
        assert!(
            stats.techniques.iter().any(|t| t == "lite::diff_compressor"),
            "expected diff_compressor technique, got: {:?}",
            stats.techniques
        );
        let compressed = messages[0].content.as_ref().and_then(|c| c.as_str()).unwrap();
        assert!(
            compressed.len() < original_len,
            "compressed ({}) should be smaller than original ({})",
            compressed.len(),
            original_len
        );
    }

    #[test]
    fn test_lite_routes_build_log_to_log_compressor() {
        // A tool result containing a 60-line pytest output should trigger
        // LogCompressor via the content router. We need ≥2 build-output
        // patterns: pytest banner + ≥5 lines with error/fail keywords.
        let mut log = String::from("===== test session starts =====\n");
        for i in 0..50 {
            log.push_str(&format!("test_module_{} PASSED\n", i));
        }
        // 5 FAILED lines satisfy the "generic ≥5 error-token lines" pattern
        log.push_str("test_critical FAILED\n");
        log.push_str("test_other FAILED\n");
        log.push_str("test_another FAILED\n");
        log.push_str("test_yet_another FAILED\n");
        log.push_str("test_last FAILED\n");
        log.push_str("test_result: 50 passed, 5 failed\n");
        let original_len = log.len();
        let mut messages = vec![OpenAIMessage {
            role: "tool".into(),
            content: Some(Value::String(log)),
            name: None,
            tool_call_id: Some("call_1".into()),
            tool_calls: None,
            extra: Default::default(),
        }];
        let stats = apply_compression(&mut messages, CompressionMode::Lite);
        assert!(
            stats.techniques.iter().any(|t| t == "lite::log_compressor"),
            "expected log_compressor technique, got: {:?}",
            stats.techniques
        );
        let compressed = messages[0].content.as_ref().and_then(|c| c.as_str()).unwrap();
        assert!(
            compressed.len() < original_len,
            "compressed ({}) should be smaller than original ({})",
            compressed.len(),
            original_len
        );
    }

    #[test]
    fn test_lite_skips_small_content() {
        // Content under 500 bytes should not be routed (not worth the
        // detection overhead). Lite's basic normalization may still apply.
        let mut messages = vec![msg("tool", "{\"a\":1}")];
        let stats = apply_compression(&mut messages, CompressionMode::Lite);
        assert!(
            !stats
                .techniques
                .iter()
                .any(|t| t.contains("smart_crusher") || t.contains("log_compressor") || t.contains("diff_compressor")),
            "small content should not trigger content routing, got: {:?}",
            stats.techniques
        );
    }

    #[test]
    fn test_lite_does_not_route_user_messages() {
        // User messages must never be compressed by the content router
        // (they're the operator's intent).
        let mut array = Vec::new();
        for i in 0..20 {
            array.push(serde_json::json!({"id": i, "name": format!("item{}", i)}));
        }
        let json_content = serde_json::to_string(&array).unwrap();
        let mut messages = vec![msg("user", &json_content)];
        let stats = apply_compression(&mut messages, CompressionMode::Lite);
        assert!(
            !stats.techniques.iter().any(|t| t.contains("smart_crusher")),
            "user messages should not be routed to smart_crusher, got: {:?}",
            stats.techniques
        );
    }
}
