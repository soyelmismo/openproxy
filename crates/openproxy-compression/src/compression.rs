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

use crate::{content_router, lite, rtk, stats};
use stats::CompressionStats;

pub use openproxy_types::CompressionMode;
use openproxy_types::OpenAIMessage;

/// Trait for applying compression to a collection of messages.
pub trait TextCompressor {
    /// Compresses `messages` in-place and returns the list of technique identifiers applied.
    fn compress(&self, messages: &mut Vec<OpenAIMessage>) -> Vec<String>;
}

pub struct LiteCompressor;
impl TextCompressor for LiteCompressor {
    fn compress(&self, messages: &mut Vec<OpenAIMessage>) -> Vec<String> {
        lite::apply_lite(messages)
            .into_iter()
            .map(std::string::ToString::to_string)
            .collect()
    }
}

pub struct RtkCompressor;
impl TextCompressor for RtkCompressor {
    fn compress(&self, messages: &mut Vec<OpenAIMessage>) -> Vec<String> {
        let mut techniques = content_router::apply_content_routing(messages);
        techniques.extend(rtk::apply_rtk(messages));
        techniques
    }
}

pub struct LiteRtkCompressor;
impl TextCompressor for LiteRtkCompressor {
    fn compress(&self, messages: &mut Vec<OpenAIMessage>) -> Vec<String> {
        let mut techniques = lite::apply_lite(messages)
            .into_iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>();
        techniques.extend(content_router::apply_content_routing(messages));
        techniques.extend(rtk::apply_rtk(messages));
        techniques
    }
}

impl TextCompressor for CompressionMode {
    fn compress(&self, messages: &mut Vec<OpenAIMessage>) -> Vec<String> {
        match self {
            CompressionMode::Off => Vec::new(),
            CompressionMode::Lite => LiteCompressor.compress(messages),
            CompressionMode::Rtk => RtkCompressor.compress(messages),
            CompressionMode::LiteRtk => LiteRtkCompressor.compress(messages),
        }
    }
}

/// Helper that measures character and token counts before and after running a `TextCompressor`,
/// returning unified `CompressionStats`.
pub fn measure_compression<C: TextCompressor>(
    messages: &mut Vec<OpenAIMessage>,
    compressor: &C,
) -> CompressionStats {
    let original_chars = count_content_chars(messages);
    let original_tokens = crate::token_estimate::estimate_prompt_tokens(messages) as usize;

    let techniques = compressor.compress(messages);

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

/// Aplica compresión a los mensajes del request según el modo.
///
/// Modifica `messages` in-place y retorna estadísticas de la compresión.
/// Retorna true si la compresión aplicaría algún cambio. Evita clonación profunda.
pub fn would_compress(messages: &[OpenAIMessage], mode: CompressionMode) -> bool {
    match mode {
        CompressionMode::Off => false,
        CompressionMode::Lite | CompressionMode::Rtk | CompressionMode::LiteRtk => {
            let chars = count_content_chars(messages);
            chars > 1000
        }
    }
}

pub fn apply_compression(
    messages: &mut Vec<OpenAIMessage>,
    mode: CompressionMode,
) -> CompressionStats {
    if mode == CompressionMode::Off {
        return CompressionStats::empty();
    }
    measure_compression(messages, &mode)
}

/// Cuenta chars totales del contenido textual de los mensajes.
fn count_content_chars(msgs: &[OpenAIMessage]) -> usize {
    msgs.iter().map(|m| m.extract_text_cow().len()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_types::OpenAIMessage;
    use serde_json::Value;
    use std::fmt::Write;

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
            "expected at least one lite:: technique, got: {techniques:?}"
        );
        // rtk rules are formatted as "{filter_id}::{rule}" where filter_id is
        // e.g. "git-status" or "generic". Distinguish them from lite rules
        // by requiring a non-lite prefix.
        assert!(
            techniques.iter().any(|t| !t.starts_with("lite::")),
            "expected at least one non-lite (rtk-derived) technique, got: {techniques:?}"
        );
        assert!(stats.compressed_chars <= stats.original_chars);
    }

    #[test]
    fn test_lite_is_strictly_lossless() {
        let long_code = "fn important() {\n    println!(\"hello\");\n}\n".repeat(200);
        let mut messages = vec![
            msg("system", "sys prompt"),
            msg("system", "sys prompt"),
            msg("user", "line1\n\n\n\nline2"),
            OpenAIMessage {
                role: "tool".into(),
                content: Some(Value::String(long_code.clone())),
                name: None,
                tool_call_id: Some("call_1".into()),
                tool_calls: None,
                extra: serde_json::Map::default(),
            },
        ];
        let stats = apply_compression(&mut messages, CompressionMode::Lite);
        // dedup_system applied
        assert_eq!(messages.len(), 3);
        // whitespace collapsed on user message
        assert_eq!(
            messages[1]
                .content
                .as_ref()
                .and_then(|c| c.as_str())
                .unwrap(),
            "line1\n\nline2"
        );
        // tool message must be 100% untouched
        let tool_result = messages[2]
            .content
            .as_ref()
            .and_then(|c| c.as_str())
            .unwrap();
        assert_eq!(tool_result, &long_code);
        assert!(!stats.techniques.iter().any(|t| t.contains("smart_crusher")
            || t.contains("diff_compressor")
            || t.contains("truncated")));
    }

    #[test]
    fn test_rtk_routes_json_array_to_smart_crusher() {
        // A tool result containing a JSON array of 20 homogeneous items
        // should trigger SmartCrusher via the content router in RTK mode.
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
            extra: serde_json::Map::default(),
        }];
        let stats = apply_compression(&mut messages, CompressionMode::Rtk);
        assert!(
            stats
                .techniques
                .iter()
                .any(|t| t == "lite::smart_crusher_lossless"),
            "expected smart_crusher_lossless technique, got: {:?}",
            stats.techniques
        );
        let compressed = messages[0]
            .content
            .as_ref()
            .and_then(|c| c.as_str())
            .unwrap();
        assert!(
            compressed.len() < original_len,
            "compressed ({}) should be smaller than original ({})",
            compressed.len(),
            original_len
        );
        // CSV schema marker should be present
        assert!(
            compressed.contains("#schema:"),
            "expected CSV schema header"
        );
    }

    #[test]
    fn test_rtk_routes_git_diff_to_diff_compressor() {
        // A tool result containing a 40-line git diff should trigger
        // DiffCompressor via the content router in RTK mode.
        let mut diff = String::from("diff --git a/foo.rs b/foo.rs\n");
        diff.push_str("index abc..def 100644\n");
        diff.push_str("--- a/foo.rs\n");
        diff.push_str("+++ b/foo.rs\n");
        diff.push_str("@@ -1,40 +1,40 @@\n");
        for i in 0..40 {
            let _ = writeln!(diff, " context line {i}");
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
            extra: serde_json::Map::default(),
        }];
        let stats = apply_compression(&mut messages, CompressionMode::Rtk);
        assert!(
            stats
                .techniques
                .iter()
                .any(|t| t == "lite::diff_compressor"),
            "expected diff_compressor technique, got: {:?}",
            stats.techniques
        );
        let compressed = messages[0]
            .content
            .as_ref()
            .and_then(|c| c.as_str())
            .unwrap();
        assert!(
            compressed.len() < original_len,
            "compressed ({}) should be smaller than original ({})",
            compressed.len(),
            original_len
        );
    }

    #[test]
    fn test_rtk_routes_build_log_to_log_compressor() {
        // A tool result containing a 60-line pytest output should trigger
        // LogCompressor via the content router in RTK mode. We need ≥2 build-output
        // patterns: pytest banner + ≥5 lines with error/fail keywords.
        let mut log = String::from("===== test session starts =====\n");
        for i in 0..50 {
            let _ = writeln!(log, "test_module_{i} PASSED");
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
            extra: serde_json::Map::default(),
        }];
        let stats = apply_compression(&mut messages, CompressionMode::Rtk);
        assert!(
            stats.techniques.iter().any(|t| t == "lite::log_compressor"),
            "expected log_compressor technique, got: {:?}",
            stats.techniques
        );
        let compressed = messages[0]
            .content
            .as_ref()
            .and_then(|c| c.as_str())
            .unwrap();
        assert!(
            compressed.len() < original_len,
            "compressed ({}) should be smaller than original ({})",
            compressed.len(),
            original_len
        );
    }

    #[test]
    fn test_rtk_skips_small_content() {
        // Content under 500 bytes should not be routed (not worth the
        // detection overhead).
        let mut messages = vec![msg("tool", "{\"a\":1}")];
        let stats = apply_compression(&mut messages, CompressionMode::Rtk);
        assert!(
            !stats.techniques.iter().any(|t| t.contains("smart_crusher")
                || t.contains("log_compressor")
                || t.contains("diff_compressor")),
            "small content should not trigger content routing, got: {:?}",
            stats.techniques
        );
    }

    #[test]
    fn test_rtk_does_not_route_user_messages() {
        // User messages must never be compressed by the content router
        // (they're the operator's intent).
        let mut array = Vec::new();
        for i in 0..20 {
            array.push(serde_json::json!({"id": i, "name": format!("item{}", i)}));
        }
        let json_content = serde_json::to_string(&array).unwrap();
        let mut messages = vec![msg("user", &json_content)];
        let stats = apply_compression(&mut messages, CompressionMode::Rtk);
        assert!(
            !stats.techniques.iter().any(|t| t.contains("smart_crusher")),
            "user messages should not be routed to smart_crusher, got: {:?}",
            stats.techniques
        );
    }
}
