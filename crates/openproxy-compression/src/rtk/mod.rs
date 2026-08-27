//! RTK (Reduced Token Kit) — Command-aware compression engine.
//!
//! Detecta el tipo de comando CLI en el contenido del mensaje y aplica
//! filtros declarativos para eliminar ruido, deduplicar y truncar.

pub mod command_detector;
pub mod line_filter;
pub mod smart_truncate;

pub use line_filter::{RTK_RULES, RtkCommandRule};

use crate::visitor::mutate_message_text;
use line_filter::{apply_line_filter, get_builtin_filter, get_generic_filter};
use openproxy_types::OpenAIMessage;

/// Aplica compresión RTK a los mensajes. Retorna las técnicas aplicadas.
///
/// Each builtin filter and the generic filter are looked up from
/// `std::sync::LazyLock` statics (built once at first use, then shared
/// via `Arc<CompiledFilter>`). `apply_line_filter` returns
/// `Vec<&'static str>` rule names — converted to `Vec<String>` at this
/// boundary to preserve the `Vec<String>` API expected by
/// `compression::apply_compression`.
fn compress_with_filter(
    content_str: &str,
    filter: &line_filter::CompiledFilter,
    techniques: &mut Vec<String>,
) -> Option<String> {
    let (filtered, rules) = apply_line_filter(content_str, filter);
    if filtered != content_str {
        techniques.extend(rules.into_iter().map(String::from));
        Some(filtered)
    } else {
        None
    }
}

fn compress_rtk_content(content_str: &str, techniques: &mut Vec<String>) -> Option<String> {
    if content_str.trim().is_empty() || content_str.len() < 20 {
        return None;
    }

    let detection = command_detector::detect(content_str);
    if detection.id != "unknown"
        && let Some(filter) = get_builtin_filter(&detection.id)
        && let Some(filtered) = compress_with_filter(content_str, &filter, techniques)
    {
        return Some(filtered);
    }

    compress_with_filter(content_str, &get_generic_filter(), techniques)
}

fn process_rtk_message(msg: &mut OpenAIMessage, all_techniques: &mut Vec<String>) {
    if msg.role != "user" && msg.role != "tool" {
        return;
    }

    let mut msg_techniques = Vec::new();
    if mutate_message_text(msg, |text| compress_rtk_content(text, &mut msg_techniques)) {
        all_techniques.extend(msg_techniques);
    }
}

/// Aplica compresión RTK a los mensajes. Retorna las técnicas aplicadas.
///
/// Each builtin filter and the generic filter are looked up from
/// `std::sync::LazyLock` statics (built once at first use, then shared
/// via `Arc<CompiledFilter>`). `apply_line_filter` returns
/// `Vec<&'static str>` rule names — converted to `Vec<String>` at this
/// boundary to preserve the `Vec<String>` API expected by
/// `compression::apply_compression`.
pub fn apply_rtk(msgs: &mut [OpenAIMessage]) -> Vec<String> {
    let mut all_techniques: Vec<String> = Vec::new();
    for msg in msgs.iter_mut() {
        process_rtk_message(msg, &mut all_techniques);
    }
    all_techniques
}

/// Cuenta los chars totales del contenido de todos los mensajes.
pub fn count_content_chars(msgs: &[OpenAIMessage]) -> usize {
    msgs.iter()
        .filter_map(|m| m.content.as_ref())
        .filter_map(|c| c.as_str())
        .map(str::len)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_types::OpenAIMessage;
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
    fn test_rtk_git_status_compression() {
        let mut msgs = vec![msg(
            "user",
            "On branch main\n  (use \"git add\" to update)\n\tmodified: foo.rs\nnothing added to commit\n",
        )];
        let techniques = apply_rtk(&mut msgs);
        assert!(!techniques.is_empty());
        let result = msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap();
        assert!(result.contains("On branch main"));
        assert!(result.contains("modified: foo.rs"));
        assert!(!result.contains("use \"git add\""));
    }

    #[test]
    fn test_rtk_empty_content_skipped() {
        let mut msgs = vec![msg("user", "")];
        let techniques = apply_rtk(&mut msgs);
        assert!(techniques.is_empty());
    }

    #[test]
    fn test_rtk_skips_assistant_messages() {
        let mut msgs = vec![msg(
            "assistant",
            "some long text that would normally be filtered",
        )];
        let techniques = apply_rtk(&mut msgs);
        assert!(techniques.is_empty());
    }
}
