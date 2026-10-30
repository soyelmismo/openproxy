use super::*;

#[test]
fn test_format_responses_tools_preserves_and_defaults_strict_false() {
    let tools = vec![
        json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Execute shell command",
                "parameters": { "type": "object" }
            }
        }),
        json!({
            "type": "function",
            "name": "read",
            "description": "Read file",
            "parameters": { "type": "object" },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "write",
            "description": "Write file",
            "parameters": { "type": "object" }
        }),
    ];

    let formatted = format_responses_tools(Some(&tools)).expect("formatted tools");
    let arr = formatted.as_array().expect("array");
    assert_eq!(arr.len(), 3);

    // Nested function tool defaults strict to false
    assert_eq!(arr[0]["name"], "bash");
    assert_eq!(arr[0]["strict"], false);

    // Flat function tool with strict: true preserves it
    assert_eq!(arr[1]["name"], "read");
    assert_eq!(arr[1]["strict"], true);

    // Flat function tool without strict defaults to false
    assert_eq!(arr[2]["name"], "write");
    assert_eq!(arr[2]["strict"], false);
}

#[test]
fn test_messages_to_responses_input_preserves_encrypted_reasoning() {
    let mut extra = serde_json::Map::new();
    extra.insert(
        "reasoning_encrypted_content".to_string(),
        Value::String("gAAAAABsecret_token".to_string()),
    );
    extra.insert(
        "reasoning_id".to_string(),
        Value::String("rs_abc123".to_string()),
    );
    extra.insert(
        "reasoning_summary".to_string(),
        json!([{ "type": "summary_text", "text": "Plan: read then edit" }]),
    );

    let messages = vec![
        OpenAIMessage {
            role: "assistant".to_string(),
            content: Some(Value::String("Calling read tool...".to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![json!({
                "id": "call_1",
                "type": "function",
                "function": { "name": "read", "arguments": "{\"path\":\"foo.txt\"}" }
            })]),
            extra,
        },
        OpenAIMessage {
            role: "tool".to_string(),
            content: Some(Value::String("file content".to_string())),
            name: None,
            tool_call_id: Some("call_1".to_string()),
            tool_calls: None,
            extra: serde_json::Map::new(),
        },
    ];

    let input_val = messages_to_responses_input(&messages).expect("ok");
    let items = input_val.as_array().expect("items array");

    // First item should be the reconstructed reasoning block
    let r_item = &items[0];
    assert_eq!(r_item["type"], "reasoning");
    assert_eq!(r_item["id"], "rs_abc123");
    assert_eq!(r_item["encrypted_content"], "gAAAAABsecret_token");
    assert_eq!(r_item["summary"][0]["text"], "Plan: read then edit");

    // Next item is assistant message
    assert_eq!(items[1]["role"], "assistant");

    // Next item is function_call
    assert_eq!(items[2]["type"], "function_call");
    assert_eq!(items[2]["call_id"], "call_1");

    // Next item is function_call_output
    assert_eq!(items[3]["type"], "function_call_output");
    assert_eq!(items[3]["call_id"], "call_1");
}

#[test]
fn test_apply_responses_reasoning_and_tier_sets_include_for_muse() {
    let mut obj = serde_json::Map::new();
    obj.insert(
        "model".to_string(),
        Value::String("muse-spark-1.3-contributor-free".to_string()),
    );

    apply_responses_reasoning_and_tier(&mut obj, None);

    assert_eq!(obj["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(obj["reasoning"]["summary"], "auto");
    assert_eq!(obj["reasoning"]["effort"], "medium");
}
