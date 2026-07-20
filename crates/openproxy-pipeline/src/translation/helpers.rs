use crate::translation::types::*;
use serde_json::Value;

pub fn message_content_to_text(content: &Option<serde_json::Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(openai_content_part_to_text)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(""),
        // `null` content (common when an assistant message carries only
        // `tool_calls` and no text) must be treated as empty, NOT as the
        // string "null" — otherwise the translator would emit a spurious
        // `{"type":"text","text":"null"}` block before the tool_use
        // blocks, which confuses Anthropic-compatible upstreams.
        Some(Value::Null) | None => String::new(),
        Some(value) => value.to_string(),
    }
}

pub fn openai_content_part_to_text(part: &serde_json::Value) -> String {
    if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
        return text.to_string();
    }

    if let Some(content) = part.get("content").and_then(|v| v.as_str()) {
        return content.to_string();
    }

    match part {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub fn parse_image_url_to_inline_data(part: &serde_json::Value) -> Option<GeminiInlineData> {
    let obj = part.as_object()?;
    if obj.get("type").and_then(|v| v.as_str())? != "image_url" {
        return None;
    }

    let url = obj.get("image_url")?.as_object()?.get("url")?.as_str()?;
    let stripped = url.strip_prefix("data:")?;
    let (mime_type, rest) = stripped.split_once(';')?;
    let (_, data) = rest.split_once(',')?;

    Some(GeminiInlineData {
        mime_type: mime_type.to_string(),
        data: data.to_string(),
    })
}

pub fn message_content_to_gemini_parts(content: &Option<serde_json::Value>) -> Vec<GeminiPart> {
    match content {
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| {
                if let Some(inline_data) = parse_image_url_to_inline_data(part) {
                    return GeminiPart {
                        inline_data: Some(inline_data),
                        ..Default::default()
                    };
                }
                GeminiPart {
                    text: Some(openai_content_part_to_text(part)),
                    ..Default::default()
                }
            })
            .collect(),
        Some(Value::Null) => vec![GeminiPart {
            text: Some(String::new()),
            ..Default::default()
        }],
        Some(value) => vec![GeminiPart {
            text: Some(value.to_string()),
            ..Default::default()
        }],
        None => vec![GeminiPart {
            text: Some(String::new()),
            ..Default::default()
        }],
    }
}
