use super::*;

fn wrapped_request(model: &str, messages: serde_json::Value) -> serde_json::Value {
    let adapter = AntigravityAdapter::new();
    let req: openproxy_types::OpenAIRequest = serde_json::from_value(json!({
        "model": model,
        "messages": messages,
    }))
    .unwrap();
    let model = ModelId::new(model);
    let target = openproxy_types::context::ResolvedTarget {
        target: openproxy_types::combos::ComboTarget::default(),
        model: openproxy_types::Model {
            model_id: model.clone(),
            ..Default::default()
        },
        api_key: String::new(),
        api_key_label: None,
        custom_meta: None,
    };
    let body = adapter
        .format_request(TargetFormat::Gemini, &req, &model, &req.messages, false)
        .unwrap();
    let body = adapter
        .wrap_request_body(body, TargetFormat::Gemini, &model, &target)
        .unwrap();
    serde_json::from_slice::<serde_json::Value>(&body).unwrap()["request"].clone()
}

#[test]
fn claude_empty_text_beside_tool_call_is_removed() {
    let request = wrapped_request(
        "claude-opus-4-6-thinking",
        json!([
            {"role": "user", "content": "Run the tool"},
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_test",
                    "type": "function",
                    "function": {"name": "calc", "arguments": "{}"}
                }]
            },
            {"role": "tool", "tool_call_id": "call_test", "content": "42"},
            {"role": "user", "content": "Continue"}
        ]),
    );
    let parts = request["contents"][1]["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 1, "empty text must not precede tool_use");
    assert_eq!(parts[0]["functionCall"]["id"], "call_test");
    assert_eq!(parts[0]["functionCall"]["name"], "calc");
    assert_eq!(
        request["contents"][2]["parts"][0]["functionResponse"]["response"]["output"],
        42
    );
}

#[test]
fn claude_empty_text_turns_keep_nonempty_parts() {
    for content in [
        json!(""),
        json!(null),
        json!([]),
        json!([{"type": "text", "text": ""}]),
    ] {
        for role in ["user", "assistant"] {
            let request = wrapped_request(
                "claude-opus-4-6-thinking",
                json!([
                    {"role": "user", "content": "Start"},
                    {"role": role, "content": content},
                    {"role": "user", "content": "Continue"}
                ]),
            );
            let contents = request["contents"].as_array().unwrap();
            assert_eq!(contents.len(), 3, "preserve conversation turns");
            let parts = contents[1]["parts"].as_array().unwrap();
            assert_eq!(parts.len(), 1);
            assert!(
                parts[0]["text"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty())
            );
        }
    }
}

#[test]
fn claude_empty_text_does_not_change_nonempty_text_or_images() {
    let request = wrapped_request(
        "claude-opus-4-6-thinking",
        json!([{
            "role": "user",
            "content": [
                {"type": "text", "text": ""},
                {"type": "text", "text": "  Hola 世界  "},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,aGVsbG8="}}
            ]
        }]),
    );
    let parts = request["contents"][0]["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["text"], "  Hola 世界  ");
    assert_eq!(parts[1]["inlineData"]["data"], "aGVsbG8=");
    assert_eq!(parts[1]["inlineData"]["mimeType"], "image/png");
}

#[test]
fn gemini_empty_text_serialization_is_unchanged() {
    let request = wrapped_request(
        "gemini-3.8-flash-high",
        json!([{"role": "user", "content": ""}]),
    );
    assert_eq!(request["contents"][0]["parts"], json!([{"text": ""}]));
}
