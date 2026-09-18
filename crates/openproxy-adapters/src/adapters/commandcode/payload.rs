use serde_json::{Value, json};

pub fn transform_openai_to_commandcode(val: &mut Value, model_name: &str) -> Value {
    let messages = val
        .get_mut("messages")
        .and_then(|m| m.as_array_mut())
        .map(std::mem::take)
        .unwrap_or_default();

    // Pass 1: index tool_call id -> name
    let mut tool_id_to_name = std::collections::HashMap::new();
    for msg in &messages {
        if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
            for tc in tool_calls {
                let id = tc.get("id").and_then(Value::as_str).unwrap_or("");
                let func = tc.get("function").unwrap_or(&Value::Null);
                let name = func.get("name").and_then(Value::as_str).unwrap_or("");
                if !id.is_empty() && !name.is_empty() {
                    tool_id_to_name.insert(id.to_string(), name.to_string());
                }
            }
        }
    }

    let mut system_prompt = String::new();
    let mut cc_messages: Vec<Value> = Vec::with_capacity(messages.len());

    for msg in messages {
        let role = msg
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user")
            .to_ascii_lowercase();

        match role.as_str() {
            "system" | "developer" => {
                let content = extract_content_string(&msg);
                if !content.is_empty() {
                    if !system_prompt.is_empty() {
                        system_prompt.push_str("\n\n");
                    }
                    system_prompt.push_str(&content);
                }
            }
            "assistant" => {
                let mut blocks: Vec<Value> = Vec::new();
                let text = match msg.get("content") {
                    Some(Value::String(s)) if !s.is_empty() => s.as_str(),
                    _ => "",
                };
                if !text.is_empty() {
                    blocks.push(json!({
                        "type": "text",
                        "text": text,
                    }));
                }

                if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
                    for tc in tool_calls {
                        let id = tc.get("id").and_then(Value::as_str).unwrap_or("");
                        let func = tc.get("function").unwrap_or(&Value::Null);
                        let name = func.get("name").and_then(Value::as_str).unwrap_or("");
                        let args = func
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}");
                        let input: Value = serde_json::from_str(args).unwrap_or_else(|_| json!({}));
                        blocks.push(json!({
                            "type": "tool-call",
                            "toolCallId": id,
                            "toolName": name,
                            "input": input,
                        }));
                    }
                }

                if blocks.is_empty() {
                    blocks.push(json!({
                        "type": "text",
                        "text": "",
                    }));
                }

                cc_messages.push(json!({
                    "role": "assistant",
                    "content": blocks,
                }));
            }
            "tool" => {
                let id = msg
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let tool_name = tool_id_to_name.get(id).map_or("", |s| s.as_str());
                let content = extract_content_string(&msg);

                cc_messages.push(json!({
                    "role": "tool",
                    "content": [
                        {
                            "type": "tool-result",
                            "toolCallId": id,
                            "toolName": tool_name,
                            "output": {
                                "type": "text",
                                "value": content,
                            }
                        }
                    ],
                }));
            }
            _ => {
                // user role
                let mut blocks: Vec<Value> = Vec::new();
                match msg.get("content") {
                    Some(Value::String(s)) => {
                        blocks.push(json!({
                            "type": "text",
                            "text": s,
                        }));
                    }
                    Some(Value::Array(arr)) => {
                        for part in arr {
                            let p_type = part.get("type").and_then(Value::as_str).unwrap_or("text");
                            if p_type == "text" {
                                let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                                blocks.push(json!({
                                    "type": "text",
                                    "text": text,
                                }));
                            } else if p_type == "image_url"
                                && let Some(url) = part
                                    .get("image_url")
                                    .and_then(|u| u.get("url"))
                                    .and_then(Value::as_str)
                            {
                                blocks.push(json!({
                                    "type": "image",
                                    "image": url,
                                }));
                            }
                        }
                    }
                    Some(v) => {
                        blocks.push(json!({
                            "type": "text",
                            "text": v.to_string(),
                        }));
                    }
                    None => {}
                }

                if blocks.is_empty() {
                    blocks.push(json!({
                        "type": "text",
                        "text": "",
                    }));
                }

                cc_messages.push(json!({
                    "role": "user",
                    "content": blocks,
                }));
            }
        }
    }

    if cc_messages.is_empty() {
        cc_messages.push(json!({
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": "",
                }
            ],
        }));
    }

    let mut params_obj = serde_json::Map::new();
    params_obj.insert("model".into(), json!(model_name));
    params_obj.insert("messages".into(), json!(cc_messages));
    params_obj.insert("stream".into(), json!(true));

    if !system_prompt.is_empty() {
        params_obj.insert("system".into(), json!(system_prompt));
    }

    if let Some(tools) = val.get("tools").and_then(Value::as_array) {
        let cc_tools: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                let func = t.get("function").or(Some(t))?;
                let name = func.get("name")?.as_str()?;
                if name.is_empty() {
                    return None;
                }
                let desc = func
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let params = func
                    .get("parameters")
                    .or_else(|| func.get("input_schema"))
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
                Some(json!({
                    "name": name,
                    "description": desc,
                    "input_schema": params,
                }))
            })
            .collect();
        if !cc_tools.is_empty() {
            params_obj.insert("tools".into(), json!(cc_tools));
        }
    }

    if let Some(tc) = val.get("tool_choice") {
        let translated = if let Some(s) = tc.as_str() {
            match s {
                "auto" => Some(json!({"type": "auto"})),
                "none" => Some(json!({"type": "none"})),
                "required" => Some(json!({"type": "any"})),
                _ => None,
            }
        } else if let Some(obj) = tc.as_object() {
            if obj.get("type").and_then(Value::as_str) == Some("function") {
                let name = obj
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !name.is_empty() {
                    Some(json!({"type": "tool", "name": name}))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        if let Some(choice) = translated {
            params_obj.insert("tool_choice".into(), choice);
        }
    }

    if let Some(max_tokens) = val
        .get("max_tokens")
        .or_else(|| val.get("max_completion_tokens"))
    {
        params_obj.insert("max_tokens".into(), max_tokens.clone());
    } else {
        params_obj.insert("max_tokens".into(), json!(64000));
    }

    if let Some(temp) = val.get("temperature") {
        params_obj.insert("temperature".into(), temp.clone());
    }

    if let Some(reasoning) = val.get("reasoning_effort") {
        params_obj.insert("reasoning_effort".into(), reasoning.clone());
    }

    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let thread_id = uuid::Uuid::new_v4().to_string();

    json!({
        "config": {
            "workingDir": "/",
            "date": today,
            "environment": "linux-x86_64, OpenProxy",
            "structure": [],
            "isGitRepo": false,
            "currentBranch": "",
            "mainBranch": "",
            "gitStatus": "",
            "recentCommits": []
        },
        "permissionMode": "standard",
        "memory": null,
        "taste": null,
        "skills": null,
        "params": params_obj,
        "threadId": thread_id,
    })
}

fn extract_content_string(msg: &Value) -> String {
    match msg.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let mut buf = String::new();
            for part in arr {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !buf.is_empty() {
                        buf.push('\n');
                    }
                    buf.push_str(text);
                }
            }
            buf
        }
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    }
}
