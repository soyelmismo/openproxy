use super::eventstream::{KiroDeframer, KiroEventType};
use openproxy_types::error::CoreError;
use openproxy_types::{OpenAIChoice, OpenAIMessage, OpenAIResponse};
use serde_json::Value;
use std::collections::BTreeMap;

/// Fold raw AWS EventStream bytes from Kiro into a single [`OpenAIResponse`].
pub fn fold_kiro_body_to_openai_response(
    body: &[u8],
    model_name: &str,
) -> Result<OpenAIResponse, CoreError> {
    let mut deframer = KiroDeframer::new();
    deframer.push(body);

    let mut content = String::new();
    let mut tool_calls_map: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut finish_reason = "stop".to_string();

    loop {
        let frame = deframer.next_frame()?;
        let Some(frame) = frame else { break };

        match frame.event_type {
            KiroEventType::AssistantResponse => {
                if let Ok(json) = serde_json::from_slice::<Value>(&frame.payload)
                    && let Some(text) = json.get("content").and_then(Value::as_str)
                {
                    content.push_str(text);
                }
            }
            KiroEventType::ToolUse => {
                if let Ok(json) = serde_json::from_slice::<Value>(&frame.payload) {
                    let tool_use_id = json
                        .get("toolUseId")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = json
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();

                    if !tool_use_id.is_empty() {
                        let entry = tool_calls_map
                            .entry(tool_use_id)
                            .or_insert_with(|| (name.clone(), String::new()));
                        if !name.is_empty() && entry.0.is_empty() {
                            entry.0 = name;
                        }
                        if let Some(input) = json.get("input").and_then(Value::as_str) {
                            entry.1.push_str(input);
                        }
                    }
                }
            }
            KiroEventType::MessageStop => {
                if let Ok(json) = serde_json::from_slice::<Value>(&frame.payload)
                    && let Some(stop) = json.get("stop").and_then(Value::as_bool)
                    && stop
                {
                    // stop observed
                }
            }
            _ => {}
        }
    }

    let mut tool_calls = Vec::new();
    if !tool_calls_map.is_empty() {
        finish_reason = "tool_calls".to_string();
        for (id, (name, args)) in tool_calls_map {
            tool_calls.push(serde_json::json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": args,
                }
            }));
        }
    }

    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());

    let message = OpenAIMessage {
        role: "assistant".to_string(),
        content: Some(Value::String(content)),
        name: None,
        tool_call_id: None,
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        extra: serde_json::Map::new(),
    };

    Ok(OpenAIResponse {
        id: format!("chatcmpl_{}", uuid::Uuid::new_v4()),
        object: "chat.completion".to_string(),
        created,
        model: model_name.to_string(),
        choices: vec![OpenAIChoice {
            index: 0,
            message,
            finish_reason: Some(finish_reason),
        }],
        usage: None,
    })
}
