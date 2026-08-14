use openproxy_types::OpenAIMessage;
use serde_json::Value;

/// Mutates the textual content of an `OpenAIMessage` transparently,
/// whether it is a plain `String` or an `Array` of content parts (`{"type":"text","text":"..."}`).
///
/// The provided `transform` closure receives a `&str` and returns `Option<String>`.
/// - Returning `Some(new_text)` indicates that the text should be replaced if it changed.
/// - Returning `None` indicates that no change is needed.
///
/// Returns `true` if any text content was mutated, `false` otherwise.
pub fn mutate_message_text<F>(msg: &mut OpenAIMessage, mut transform: F) -> bool
where
    F: FnMut(&str) -> Option<String>,
{
    let mut mutated = false;
    match &mut msg.content {
        Some(Value::String(s)) => {
            if let Some(new_s) = transform(s) {
                if &new_s != s {
                    *s = new_s;
                    mutated = true;
                }
            }
        }
        Some(Value::Array(parts)) => {
            for part in parts.iter_mut() {
                if let Some(obj) = part.as_object_mut() {
                    if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
                        if let Some(new_text) = transform(text) {
                            if new_text != text {
                                obj.insert("text".to_string(), Value::String(new_text));
                                mutated = true;
                            }
                        }
                    } else if let Some(content) = obj.get("content").and_then(|v| v.as_str()) {
                        if let Some(new_text) = transform(content) {
                            if new_text != content {
                                obj.insert("content".to_string(), Value::String(new_text));
                                mutated = true;
                            }
                        }
                    }
                } else if let Value::String(text) = part {
                    if let Some(new_text) = transform(text) {
                        if &new_text != text {
                            *part = Value::String(new_text);
                            mutated = true;
                        }
                    }
                }
            }
        }
        _ => {}
    }
    mutated
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_mutate_plain_string_content() {
        let mut msg = OpenAIMessage {
            role: "user".to_string(),
            content: Some(Value::String("hello   world".to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        };

        let changed = mutate_message_text(&mut msg, |text| Some(text.replace("   ", " ")));
        assert!(changed);
        assert_eq!(msg.content, Some(Value::String("hello world".to_string())));
    }

    #[test]
    fn test_mutate_array_parts_content() {
        let mut msg = OpenAIMessage {
            role: "user".to_string(),
            content: Some(json!([
                {"type": "text", "text": "foo   bar"},
                {"type": "image_url", "image_url": {"url": "https://example.com/img.png"}},
                {"type": "text", "text": "baz   qux"}
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        };

        let changed = mutate_message_text(&mut msg, |text| Some(text.replace("   ", " ")));
        assert!(changed);

        let parts = msg.content.as_ref().unwrap().as_array().unwrap();
        assert_eq!(parts[0]["text"], "foo bar");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[2]["text"], "baz qux");
    }

    #[test]
    fn test_no_mutation_when_unchanged() {
        let mut msg = OpenAIMessage {
            role: "user".to_string(),
            content: Some(Value::String("already clean".to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        };

        let changed = mutate_message_text(&mut msg, |_| None);
        assert!(!changed);
        assert_eq!(
            msg.content,
            Some(Value::String("already clean".to_string()))
        );
    }
}
