use crate::translation::OpenAIResponse;
use serde_json::Value;

/// Checks if a JSON key represents internal reasoning metadata that should be purged
/// when `purge_reasoning` is enabled.
pub fn is_reasoning_key(k: &str) -> bool {
    k.starts_with("reasoning") || k == "thought" || k == "thinking"
}

/// Checks if a JSON key represents provider-specific telemetry.
pub fn is_internal_telemetry_key(k: &str) -> bool {
    k == "system_fingerprint" || k == "service_tier"
}

/// Checks if a JSON key represents internal reasoning or provider-specific telemetry
/// that should be stripped when response sanitization is enabled.
pub fn is_reasoning_or_internal_key(k: &str) -> bool {
    is_reasoning_key(k) || is_internal_telemetry_key(k)
}

/// Sanitizes an unary (non-streaming) `OpenAIResponse`:
/// 1. Forces `resp.model` to match `requested_model`.
/// 2. Purges internal telemetry keys from every choice's message `extra` map.
/// 3. When `purge_reasoning` is true, also purges reasoning keys.
pub fn sanitize_unary_response(
    resp: &mut OpenAIResponse,
    requested_model: &str,
    purge_reasoning: bool,
) {
    resp.model = requested_model.to_string();
    for choice in &mut resp.choices {
        choice.message.extra.retain(|k, _| {
            if is_internal_telemetry_key(k) {
                return false;
            }
            if purge_reasoning && is_reasoning_key(k) {
                return false;
            }
            true
        });
    }
}

/// Sanitizes a streaming chunk JSON string:
/// 1. Rewrites `model` to `requested_model`.
/// 2. Purges `system_fingerprint` and `service_tier`.
/// 3. If `purge_reasoning` is true, removes reasoning keys from each choice delta.
/// 4. If `purge_reasoning` is true and all choices were pure reasoning (no content, no tool_calls, no role, no finish_reason),
///    returns `None` so the entire chunk is skipped.
pub fn sanitize_streaming_chunk(
    json_str: &str,
    requested_model: &str,
    purge_reasoning: bool,
) -> Option<String> {
    let mut val: Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return Some(json_str.to_string()),
    };

    let Some(obj) = val.as_object_mut() else {
        return Some(json_str.to_string());
    };

    // 1. Force requested model
    obj.insert(
        "model".to_string(),
        Value::String(requested_model.to_string()),
    );

    // 2. Remove root internal keys
    obj.remove("system_fingerprint");
    obj.remove("service_tier");

    // 3. Inspect choices
    if let Some(choices_val) = obj.get_mut("choices")
        && let Some(choices_arr) = choices_val.as_array_mut()
    {
        let had_choices = !choices_arr.is_empty();
        choices_arr.retain_mut(|c| {
            let Some(c_obj) = c.as_object_mut() else {
                return true;
            };

            let finish_reason = c_obj.get("finish_reason");
            let has_finish = finish_reason.is_some_and(|f| !f.is_null());

            if let Some(delta_val) = c_obj.get_mut("delta")
                && let Some(delta_obj) = delta_val.as_object_mut()
            {
                delta_obj.retain(|k, _| {
                    if is_internal_telemetry_key(k) {
                        return false;
                    }
                    if purge_reasoning && is_reasoning_key(k) {
                        return false;
                    }
                    true
                });

                let has_useful = delta_obj.contains_key("content")
                    || delta_obj.contains_key("tool_calls")
                    || delta_obj.contains_key("role")
                    || (!purge_reasoning && delta_obj.keys().any(|k| is_reasoning_key(k)));

                // If delta has no useful user-facing payload and there is no finish_reason, drop choice
                if purge_reasoning && !has_useful && !has_finish {
                    return false;
                }
            }
            true
        });

        // If the upstream chunk had choices but all were pure reasoning (e.g. Nous/Kimi thinking chunks),
        // drop the entire chunk frame by returning None only when purge_reasoning is enabled.
        if purge_reasoning && had_choices && choices_arr.is_empty() {
            return None;
        }
    }

    serde_json::to_string(&val).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_reasoning_or_internal_key() {
        assert!(is_reasoning_or_internal_key("reasoning_content"));
        assert!(is_reasoning_or_internal_key("reasoning_details"));
        assert!(is_reasoning_or_internal_key("thought"));
        assert!(is_reasoning_or_internal_key("thinking"));
        assert!(is_reasoning_or_internal_key("system_fingerprint"));
        assert!(is_reasoning_or_internal_key("service_tier"));
        assert!(!is_reasoning_or_internal_key("content"));
        assert!(!is_reasoning_or_internal_key("tool_calls"));
    }

    #[test]
    fn test_sanitize_streaming_reasoning_chunk() {
        let mercury_chunk = r#"{"id":"gen-1","object":"chat.completion.chunk","created":123,"model":"mercury-3","choices":[{"index":0,"delta":{"reasoning_details":[{"type":"reasoning.encrypted"}]},"finish_reason":null}]}"#;
        assert_eq!(sanitize_streaming_chunk(mercury_chunk, "gemini-3.5-flash-lite", true), None);
        let kept = sanitize_streaming_chunk(mercury_chunk, "gemini-3.5-flash-lite", false).unwrap();
        assert!(kept.contains(r#""model":"gemini-3.5-flash-lite""#));
        assert!(kept.contains("reasoning_details"));

        let kimi_chunk = r#"{"id":"gen-2","object":"chat.completion.chunk","created":123,"model":"moonshotai/Kimi-K3","choices":[{"index":0,"delta":{"reasoning_content":"Thinking..."},"finish_reason":null}]}"#;
        assert_eq!(sanitize_streaming_chunk(kimi_chunk, "gemini-3.5-flash-lite", true), None);
        let kept_kimi = sanitize_streaming_chunk(kimi_chunk, "gemini-3.5-flash-lite", false).unwrap();
        assert!(kept_kimi.contains(r#""model":"gemini-3.5-flash-lite""#));
        assert!(kept_kimi.contains("reasoning_content"));
    }

    #[test]
    fn test_sanitize_unary_response_preserves_reasoning_by_default() {
        let raw = serde_json::json!({
            "id": "resp-1",
            "object": "chat.completion",
            "created": 1,
            "model": "moonshotai/Kimi-K3",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello",
                    "reasoning_content": "secret thoughts",
                    "system_fingerprint": "fp_123"
                },
                "finish_reason": "stop"
            }]
        });
        let mut resp: OpenAIResponse = serde_json::from_value(raw).unwrap();

        sanitize_unary_response(&mut resp, "gemini-3.5-flash-lite", false);
        assert_eq!(resp.model, "gemini-3.5-flash-lite");
        assert!(resp.choices[0].message.extra.contains_key("reasoning_content"));
        assert!(!resp.choices[0].message.extra.contains_key("system_fingerprint"));

        let mut resp_purged: OpenAIResponse = serde_json::from_value(
            serde_json::json!({
                "id": "resp-1",
                "object": "chat.completion",
                "created": 1,
                "model": "moonshotai/Kimi-K3",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "Hello",
                        "reasoning_content": "secret thoughts",
                        "system_fingerprint": "fp_123"
                    },
                    "finish_reason": "stop"
                }]
            }),
        )
        .unwrap();
        sanitize_unary_response(&mut resp_purged, "gemini-3.5-flash-lite", true);
        assert!(resp_purged.choices[0].message.extra.is_empty());
    }

    #[test]
    fn test_sanitize_streaming_content_chunk() {
        let content_chunk = r#"{"id":"gen-1","object":"chat.completion.chunk","created":123,"model":"mercury-3","system_fingerprint":"fp_123","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#;
        let res = sanitize_streaming_chunk(content_chunk, "gemini-3.5-flash-lite", true).unwrap();
        assert!(res.contains(r#""model":"gemini-3.5-flash-lite""#));
        assert!(!res.contains("system_fingerprint"));
        assert!(res.contains(r#""content":"Hello""#));

        let res_no_purge = sanitize_streaming_chunk(content_chunk, "gemini-3.5-flash-lite", false).unwrap();
        assert!(res_no_purge.contains(r#""model":"gemini-3.5-flash-lite""#));
        assert!(!res_no_purge.contains("system_fingerprint"));
        assert!(res_no_purge.contains(r#""content":"Hello""#));
    }
}
