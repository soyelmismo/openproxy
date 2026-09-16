use crate::translation::types::AnthropicMessage;

fn collect_message_diagnostic_ids(
    m: &AnthropicMessage,
    tool_use_ids: &mut Vec<String>,
    tool_result_ids: &mut Vec<String>,
) {
    let Some(arr) = m.content.as_array() else {
        return;
    };
    for block in arr {
        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match (m.role.as_str(), block_type) {
            ("assistant", "tool_use") => {
                if let Some(id) = block.get("id").and_then(|v| v.as_str()) {
                    tool_use_ids.push(id.to_string());
                }
            }
            ("user", "tool_result") => {
                if let Some(id) = block.get("tool_use_id").and_then(|v| v.as_str()) {
                    tool_result_ids.push(id.to_string());
                }
            }
            _ => {}
        }
    }
}

fn collect_diagnostic_tool_ids(conversation: &[AnthropicMessage]) -> (Vec<String>, Vec<String>) {
    let mut tool_use_ids = Vec::new();
    let mut tool_result_ids = Vec::new();
    for m in conversation {
        collect_message_diagnostic_ids(m, &mut tool_use_ids, &mut tool_result_ids);
    }
    (tool_use_ids, tool_result_ids)
}

fn warn_consecutive_same_roles(conversation: &[AnthropicMessage]) {
    for (i, window) in conversation.windows(2).enumerate() {
        if window[0].role == window[1].role {
            tracing::warn!(
                idx = i + 1,
                role = %window[1].role,
                "translation: consecutive same-role messages — Anthropic/MiniMax rejects this with (2013)"
            );
        }
    }
}

fn log_tool_id_diagnostics(tool_use_ids: &[String], tool_result_ids: &[String]) {
    let use_set: std::collections::HashSet<&str> = tool_use_ids
        .iter()
        .map(std::string::String::as_str)
        .collect();
    let result_set: std::collections::HashSet<&str> = tool_result_ids
        .iter()
        .map(std::string::String::as_str)
        .collect();
    let missing_results: Vec<&str> = use_set.difference(&result_set).copied().collect();
    let orphan_results: Vec<&str> = result_set.difference(&use_set).copied().collect();

    if !missing_results.is_empty() || !orphan_results.is_empty() {
        tracing::warn!(
            missing_results = ?missing_results,
            orphan_results = ?orphan_results,
            "translation: tool_use/tool_result ID mismatch — MiniMax will reject with (2013)"
        );
    }
}

pub(super) fn log_anthropic_translation_diagnostics(conversation: &[AnthropicMessage]) {
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }
    let role_seq: Vec<&str> = conversation.iter().map(|m| m.role.as_str()).collect();
    let (tool_use_ids, tool_result_ids) = collect_diagnostic_tool_ids(conversation);

    tracing::debug!(
        role_sequence = ?role_seq,
        tool_use_count = tool_use_ids.len(),
        tool_result_count = tool_result_ids.len(),
        tool_use_ids = ?tool_use_ids,
        tool_result_ids = ?tool_result_ids,
        "openai_to_anthropic translation result"
    );

    log_tool_id_diagnostics(&tool_use_ids, &tool_result_ids);
    warn_consecutive_same_roles(conversation);
}

pub(super) fn translate_anthropic_tools_to_openai(
    ts: Vec<serde_json::Value>,
) -> Vec<serde_json::Value> {
    ts.into_iter()
        .map(|mut t| {
            if let Some(obj) = t.as_object_mut() {
                let mut f = serde_json::Map::new();
                if let Some(n) = obj.remove("name") {
                    f.insert("name".to_string(), n);
                }
                if let Some(d) = obj.remove("description") {
                    f.insert("description".to_string(), d);
                }
                if let Some(s) = obj.remove("input_schema") {
                    f.insert("parameters".to_string(), s);
                }
                serde_json::json!({
                    "type": "function",
                    "function": f
                })
            } else {
                t
            }
        })
        .collect()
}

pub(super) fn translate_anthropic_tool_choice_to_openai(
    tc: serde_json::Value,
) -> serde_json::Value {
    if let Some(obj) = tc.as_object()
        && obj.get("type").and_then(|v| v.as_str()) == Some("tool")
        && let Some(name) = obj.get("name")
    {
        return serde_json::json!({
            "type": "function",
            "function": { "name": name }
        });
    }
    tc
}
