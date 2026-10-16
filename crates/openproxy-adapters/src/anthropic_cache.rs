use serde_json::{Value, json};

/// Checks if a JSON value represents a valid Claude cache_control object.
pub fn is_valid_cache_control(cc: &Value) -> bool {
    let Some(obj) = cc.as_object() else {
        return false;
    };
    obj.get("type").and_then(Value::as_str) == Some("ephemeral")
}

/// Checks if an object has a `cache_control` field.
pub fn has_cache_control(val: &Value) -> bool {
    val.get("cache_control").is_some()
}

/// Counts total cache_control blocks present across tools, system, and messages.
pub fn count_cache_controls(payload: &Value) -> usize {
    let mut count = 0;

    if let Some(tools) = payload.get("tools").and_then(Value::as_array) {
        for t in tools {
            if has_cache_control(t) {
                count += 1;
            }
        }
    }

    if let Some(system) = payload.get("system").and_then(Value::as_array) {
        for s in system {
            if has_cache_control(s) {
                count += 1;
            }
        }
    }

    if let Some(messages) = payload.get("messages").and_then(Value::as_array) {
        for m in messages {
            if let Some(content) = m.get("content").and_then(Value::as_array) {
                for block in content {
                    if has_cache_control(block) {
                        count += 1;
                    }
                }
            }
        }
    }

    count
}

/// Reports whether the payload has a system prompt that can host a cache breakpoint.
pub fn claude_payload_has_cacheable_system(payload: &Value) -> bool {
    match payload.get("system") {
        None => false,
        Some(Value::Array(arr)) => !arr.is_empty(),
        Some(Value::String(s)) => !s.trim().is_empty(),
        _ => false,
    }
}

/// Injects cache_control onto the last non-deferred tool in the tools array.
/// Only adds cache_control if NO tool in the array already has one.
pub fn inject_tools_cache_control(payload: &mut Value) {
    let Some(tools) = payload.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    if tools.is_empty() {
        return;
    }

    let mut has_cc = false;
    let mut last_eligible = None;
    for (idx, tool) in tools.iter().enumerate() {
        if has_cache_control(tool) {
            has_cc = true;
            break;
        }
        let is_deferred = tool
            .get("defer_loading")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !is_deferred {
            last_eligible = Some(idx);
        }
    }

    if has_cc {
        return;
    }

    if let Some(idx) = last_eligible
        && let Some(obj) = tools[idx].as_object_mut()
    {
        obj.insert("cache_control".to_string(), json!({"type": "ephemeral"}));
    }
}

/// Injects cache_control to the last element in the system prompt.
/// Converts string system prompts to array format if needed.
/// Only adds cache_control if NO system element already has one.
pub fn inject_system_cache_control(payload: &mut Value) {
    let Some(system) = payload.get_mut("system") else {
        return;
    };

    if let Some(arr) = system.as_array_mut() {
        if arr.is_empty() {
            return;
        }
        let has_cc = arr.iter().any(has_cache_control);
        if has_cc {
            return;
        }
        if let Some(last_item) = arr.last_mut()
            && let Some(obj) = last_item.as_object_mut()
        {
            obj.insert("cache_control".to_string(), json!({"type": "ephemeral"}));
        }
    } else if let Some(s) = system.as_str() {
        if s.trim().is_empty() {
            return;
        }
        let text = s.to_string();
        *system = json!([{
            "type": "text",
            "text": text,
            "cache_control": {
                "type": "ephemeral"
            }
        }]);
    }
}

fn is_message_eligible_for_rolling_cache(msg: &Value) -> bool {
    let Some(content) = msg.get("content") else {
        return false;
    };
    if content.is_string() {
        return true;
    }
    let Some(arr) = content.as_array() else {
        return false;
    };
    if arr.is_empty() {
        return false;
    }

    let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
    if role != "assistant" {
        return true;
    }

    let Some(last_block) = arr.last() else {
        return false;
    };
    let typ = last_block.get("type").and_then(Value::as_str).unwrap_or("");
    typ != "thinking" && typ != "redacted_thinking"
}

fn message_content_has_cache_control(content: &Value) -> bool {
    if let Some(arr) = content.as_array() {
        arr.iter().any(has_cache_control)
    } else {
        false
    }
}

/// Injects a rolling cache_control marker into the message selected by the native
/// Claude Code selector (latest eligible user/assistant message).
pub fn inject_messages_cache_control(payload: &mut Value) {
    let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    if messages.is_empty() {
        return;
    }

    let last_message_index = messages.len() - 1;
    let mut last_eligible_index = None;
    for (idx, msg) in messages.iter().enumerate() {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        if role != "user" && role != "assistant" {
            continue;
        }
        if is_message_eligible_for_rolling_cache(msg) {
            last_eligible_index = Some(idx);
        }
    }

    // Native Claude Code special case: trailing system message with non-empty string content
    if last_eligible_index.is_some() {
        let final_msg = &messages[last_message_index];
        let role = final_msg.get("role").and_then(Value::as_str).unwrap_or("");
        if role == "system"
            && let Some(s) = final_msg.get("content").and_then(Value::as_str)
            && !s.trim().is_empty()
        {
            let text = s.to_string();
            messages[last_message_index]["content"] = json!([{
                "type": "text",
                "text": text,
                "cache_control": {
                    "type": "ephemeral"
                }
            }]);
            return;
        }
    }

    let Some(eligible_idx) = last_eligible_index else {
        return;
    };

    let msg = &mut messages[eligible_idx];
    if message_content_has_cache_control(&msg["content"]) {
        return;
    }

    if let Some(arr) = msg.get_mut("content").and_then(Value::as_array_mut) {
        if let Some(last_block) = arr.last_mut()
            && let Some(obj) = last_block.as_object_mut()
        {
            obj.insert("cache_control".to_string(), json!({"type": "ephemeral"}));
        }
    } else if let Some(s) = msg.get("content").and_then(Value::as_str) {
        let text = s.to_string();
        msg["content"] = json!([{
            "type": "text",
            "text": text,
            "cache_control": {
                "type": "ephemeral"
            }
        }]);
    }
}

/// Injects default cache_control breakpoints for Anthropic payloads:
/// 1. System prompt (or last non-deferred tool if system prompt is absent/empty).
/// 2. Rolling latest eligible message.
pub fn ensure_anthropic_cache_control(payload: &mut Value) {
    if !claude_payload_has_cacheable_system(payload) {
        inject_tools_cache_control(payload);
    }
    inject_system_cache_control(payload);
    inject_messages_cache_control(payload);
}

/// Enforces the Anthropic API breakpoint limit (typically 4, or 3 if thread exists).
/// Strips lowest-value breakpoints first:
/// Phase 1: earliest system blocks (preserving the last one).
/// Phase 2: earliest tool blocks (preserving the last one).
/// Phase 3: message content blocks earliest-first.
/// Phase 4: remaining system blocks.
/// Phase 5: remaining tool blocks.
pub fn enforce_cache_control_limit(payload: &mut Value, mut max_blocks: usize) {
    if payload.get("thread").is_some_and(|t| !t.is_null()) && max_blocks > 0 {
        max_blocks -= 1;
    }

    let total = count_cache_controls(payload);
    if total <= max_blocks {
        return;
    }

    let mut excess = total - max_blocks;

    // Phase 1: system blocks earliest-first, preserving the last one
    if let Some(system) = payload.get_mut("system").and_then(Value::as_array_mut) {
        let last_sys_cc = system.iter().rposition(has_cache_control);
        if let Some(last_idx) = last_sys_cc {
            for (idx, item) in system.iter_mut().enumerate() {
                if excess == 0 {
                    break;
                }
                if idx == last_idx {
                    continue;
                }
                if let Some(obj) = item.as_object_mut()
                    && obj.remove("cache_control").is_some()
                {
                    excess -= 1;
                }
            }
        }
    }
    if excess == 0 {
        return;
    }

    // Phase 2: tool blocks earliest-first, preserving the last one
    if let Some(tools) = payload.get_mut("tools").and_then(Value::as_array_mut) {
        let last_tool_cc = tools.iter().rposition(has_cache_control);
        if let Some(last_idx) = last_tool_cc {
            for (idx, tool) in tools.iter_mut().enumerate() {
                if excess == 0 {
                    break;
                }
                if idx == last_idx {
                    continue;
                }
                if let Some(obj) = tool.as_object_mut()
                    && obj.remove("cache_control").is_some()
                {
                    excess -= 1;
                }
            }
        }
    }
    if excess == 0 {
        return;
    }

    // Phase 3: message content blocks earliest-first
    if let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) {
        for msg in messages.iter_mut() {
            if excess == 0 {
                break;
            }
            if let Some(content) = msg.get_mut("content").and_then(Value::as_array_mut) {
                for block in content.iter_mut() {
                    if excess == 0 {
                        break;
                    }
                    if let Some(obj) = block.as_object_mut()
                        && obj.remove("cache_control").is_some()
                    {
                        excess -= 1;
                    }
                }
            }
        }
    }
    if excess == 0 {
        return;
    }

    // Phase 4: remaining system blocks (last system)
    if let Some(system) = payload.get_mut("system").and_then(Value::as_array_mut) {
        for item in system.iter_mut() {
            if excess == 0 {
                break;
            }
            if let Some(obj) = item.as_object_mut()
                && obj.remove("cache_control").is_some()
            {
                excess -= 1;
            }
        }
    }
    if excess == 0 {
        return;
    }

    // Phase 5: remaining tool blocks (last tool)
    if let Some(tools) = payload.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools.iter_mut() {
            if excess == 0 {
                break;
            }
            if let Some(obj) = tool.as_object_mut()
                && obj.remove("cache_control").is_some()
            {
                excess -= 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_valid_cache_control() {
        assert!(is_valid_cache_control(&json!({"type": "ephemeral"})));
        assert!(!is_valid_cache_control(&json!({"type": "persistent"})));
        assert!(!is_valid_cache_control(&json!({"other": "field"})));
        assert!(!is_valid_cache_control(&json!("ephemeral")));
        assert!(!is_valid_cache_control(&json!(null)));
    }

    #[test]
    fn test_string_system_prompt_gets_cache_control() {
        let mut input = json!({
            "model": "claude-3-5-sonnet",
            "system": "This is a long system prompt",
            "messages": []
        });
        ensure_anthropic_cache_control(&mut input);

        assert_eq!(
            input["system"][0]["cache_control"]["type"].as_str(),
            Some("ephemeral")
        );
    }

    #[test]
    fn test_array_system_prompt_last_element_gets_cache_control() {
        let mut input = json!({
            "model": "claude-3-5-sonnet",
            "system": [
                {"type": "text", "text": "Part 1"},
                {"type": "text", "text": "Part 2"}
            ],
            "messages": []
        });
        ensure_anthropic_cache_control(&mut input);

        assert!(input["system"][0].get("cache_control").is_none());
        assert_eq!(
            input["system"][1]["cache_control"]["type"].as_str(),
            Some("ephemeral")
        );
    }

    #[test]
    fn test_tools_not_auto_cached_when_system_present() {
        let mut input = json!({
            "model": "claude-3-5-sonnet",
            "tools": [
                {"name": "tool1", "description": "First tool"},
                {"name": "tool2", "description": "Second tool"}
            ],
            "system": "System prompt",
            "messages": []
        });
        ensure_anthropic_cache_control(&mut input);

        assert!(input["tools"][0].get("cache_control").is_none());
        assert!(input["tools"][1].get("cache_control").is_none());
        assert_eq!(
            input["system"][0]["cache_control"]["type"].as_str(),
            Some("ephemeral")
        );
    }

    #[test]
    fn test_tools_fallback_when_system_absent() {
        let mut input = json!({
            "model": "claude-3-5-sonnet",
            "tools": [
                {"name": "tool1", "description": "Tool 1"},
                {"name": "tool2", "description": "Tool 2"}
            ],
            "messages": [{"role": "user", "content": "Hi"}]
        });
        ensure_anthropic_cache_control(&mut input);

        assert!(input["tools"][0].get("cache_control").is_none());
        assert_eq!(
            input["tools"][1]["cache_control"]["type"].as_str(),
            Some("ephemeral")
        );
        assert_eq!(
            input["messages"][0]["content"][0]["cache_control"]["type"].as_str(),
            Some("ephemeral")
        );
    }

    #[test]
    fn test_messages_caching_latest_user_rolling_advances() {
        let mut input = json!({
            "model": "claude-3-5-sonnet",
            "messages": [
                {"role": "user", "content": "First user"},
                {"role": "assistant", "content": "Assistant reply"},
                {"role": "user", "content": "Second user"},
                {"role": "assistant", "content": "Assistant reply 2"},
                {"role": "user", "content": "Third user"}
            ]
        });
        ensure_anthropic_cache_control(&mut input);

        assert!(input["messages"][0].get("cache_control").is_none());
        assert!(input["messages"][2].get("cache_control").is_none());
        assert_eq!(
            input["messages"][4]["content"][0]["cache_control"]["type"].as_str(),
            Some("ephemeral")
        );
    }

    #[test]
    fn test_messages_skip_trailing_thinking() {
        let mut input = json!({
            "messages": [
                {"role": "user", "content": "User prompt"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Assistant reply"},
                    {"type": "thinking", "thinking": "Internal thought"}
                ]}
            ]
        });
        ensure_anthropic_cache_control(&mut input);

        // Assistant trailing thinking block is ineligible, so fallback to user prompt
        assert_eq!(
            input["messages"][0]["content"][0]["cache_control"]["type"].as_str(),
            Some("ephemeral")
        );
        assert!(
            input["messages"][1]["content"][1]
                .get("cache_control")
                .is_none()
        );
    }

    #[test]
    fn test_enforce_cache_control_limit_strips_excess() {
        let mut input = json!({
            "tools": [
                {"name": "t1", "cache_control": {"type": "ephemeral"}},
                {"name": "t2", "cache_control": {"type": "ephemeral"}}
            ],
            "system": [
                {"type": "text", "text": "s1", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "s2", "cache_control": {"type": "ephemeral"}}
            ],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "u1", "cache_control": {"type": "ephemeral"}},
                    {"type": "text", "text": "u2", "cache_control": {"type": "ephemeral"}}
                ]}
            ]
        });
        assert_eq!(count_cache_controls(&input), 6);

        enforce_cache_control_limit(&mut input, 4);
        assert_eq!(count_cache_controls(&input), 4);

        // Phase 1 removes s1 (earliest system, preserving s2)
        assert!(input["system"][0].get("cache_control").is_none());
        assert!(input["system"][1].get("cache_control").is_some());

        // Phase 2 removes t1 (earliest tool, preserving t2)
        assert!(input["tools"][0].get("cache_control").is_none());
        assert!(input["tools"][1].get("cache_control").is_some());
    }
}
