/// Compression pipeline: Lite + RTK modes.
///
/// # Modes
/// - `Off`: No compression, zero overhead.
/// - `Lite`: 5 deterministic text-normalization techniques, zero semantic change.
/// - `Rtk`: Command-aware filtering for CLI tool output (git, test, build, etc.).

pub mod lite;
pub mod rtk;
pub mod stats;

use crate::translation::OpenAIMessage;
use serde::{Deserialize, Serialize};
use stats::CompressionStats;

/// Modo de compresión.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompressionMode {
    Off,
    Lite,
    Rtk,
    LiteRtk,
}

impl Default for CompressionMode {
    fn default() -> Self {
        Self::Off
    }
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
            let original = count_content_chars(messages);
            let lite_techniques = lite::apply_lite(messages);
            let techniques: Vec<String> =
                lite_techniques.iter().map(|s| (*s).to_string()).collect();
            let compressed = count_content_chars(messages);
            CompressionStats::new(original, compressed, techniques)
        }
        CompressionMode::Rtk => {
            let original = count_content_chars(messages);
            let techniques = rtk::apply_rtk(messages);
            let compressed = count_content_chars(messages);
            CompressionStats::new(original, compressed, techniques)
        }
        CompressionMode::LiteRtk => {
            let original = count_content_chars(messages);
            let lite_techniques = lite::apply_lite(messages);
            let mut techniques: Vec<String> =
                lite_techniques.iter().map(|s| (*s).to_string()).collect();
            techniques.extend(rtk::apply_rtk(messages));
            let compressed = count_content_chars(messages);
            CompressionStats::new(original, compressed, techniques)
        }
    }
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
}
