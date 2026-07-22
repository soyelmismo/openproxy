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
