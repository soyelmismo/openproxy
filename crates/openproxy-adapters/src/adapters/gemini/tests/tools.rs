use super::super::*;
use serde_json::json;

#[test]
fn test_translate_openai_tools_flat_format() {
    let tool = json!({
        "type": "function",
        "function": { "name": "get_weather", "description": "Get current weather", "parameters": { "type": "object", "properties": { "city": {"type": "string"} } } }
    });
    let (tools, _) = translate_openai_tools_to_gemini(Some(&[tool]), None);
    let tools = tools.expect("expected tools");
    assert_eq!(tools.len(), 1);
    let decl = &tools[0].function_declarations[0];
    assert_eq!(
        (decl.name.as_str(), decl.description.as_deref()),
        ("get_weather", Some("Get current weather"))
    );
    let params = decl.parameters.as_ref().expect("parameters present");
    assert_eq!(params["type"], "object");
    assert_eq!(params["properties"]["city"]["type"], "string");
}

#[test]
fn test_translate_openai_tools_nested_format() {
    let tool = json!({ "name": "lookup", "description": "Look up data", "parameters": { "type": "object", "properties": { "id": {"type": "string"} } } });
    let (tools, _) = translate_openai_tools_to_gemini(Some(&[tool]), None);
    let decl = &tools.expect("expected tools")[0].function_declarations[0];
    assert_eq!(decl.name.as_str(), "lookup");
    assert_eq!(decl.description.as_deref(), Some("Look up data"));
    assert_eq!(decl.parameters.as_ref().unwrap()["type"], "object");
}

#[test]
fn test_tool_choice_string_modes() {
    let dummy_tool = vec![json!({"type": "function", "function": {"name": "noop"}})];
    let cases = [
        ("none", GeminiFunctionCallingMode::None),
        ("required", GeminiFunctionCallingMode::Any),
        ("auto", GeminiFunctionCallingMode::Auto),
        ("any", GeminiFunctionCallingMode::Any),
        ("unknown_mode", GeminiFunctionCallingMode::Auto),
    ];
    for (mode_str, expected) in cases {
        let (_, cfg) = translate_openai_tools_to_gemini(Some(&dummy_tool), Some(&json!(mode_str)));
        assert_eq!(cfg.unwrap().function_calling_config.mode, expected);
    }
}

#[test]
fn test_tool_choice_object_with_function_name() {
    let dummy_tool = vec![json!({
        "type": "function",
        "function": {"name": "noop"}
    })];
    let tc = json!({
        "type": "function",
        "function": {"name": "foo"}
    });
    let (_, cfg) = translate_openai_tools_to_gemini(Some(&dummy_tool), Some(&tc));
    let cfg = cfg.unwrap();
    assert!(matches!(
        cfg.function_calling_config.mode,
        GeminiFunctionCallingMode::Any
    ));
    assert_eq!(
        cfg.function_calling_config.allowed_function_names,
        Some(vec!["foo".to_string()])
    );
}

#[test]
fn test_clean_json_schema_in_parameters() {
    // Parameters with $defs and $ref — should be flattened by clean_json_schema.
    let tool = json!({
        "type": "function",
        "function": {
            "name": "addr",
            "parameters": {
                "$defs": {
                    "Address": {
                        "type": "object",
                        "properties": {
                            "city": {"type": "string"}
                        }
                    }
                },
                "type": "object",
                "properties": {
                    "home": {"$ref": "#/$defs/Address"}
                }
            }
        }
    });
    let (tools, _config) = translate_openai_tools_to_gemini(Some(&[tool]), None);
    let decl = &tools.unwrap()[0].function_declarations[0];
    let params = decl.parameters.as_ref().unwrap();
    assert_eq!(params["properties"]["home"]["type"], "object");
    assert_eq!(
        params["properties"]["home"]["properties"]["city"]["type"],
        "string"
    );
    assert!(params.get("$defs").is_none());
}

#[test]
fn test_tools_empty_or_none_returns_none() {
    for tools_arg in [None, Some(&[][..])] {
        let (tools, cfg) = translate_openai_tools_to_gemini(tools_arg, None);
        assert!(tools.is_none() && cfg.is_none());
    }
}

#[test]
fn test_tool_without_name_is_skipped() {
    let tools_in = vec![
        json!({"type": "function", "function": {"description": "no name"}}),
        json!({"type": "function", "function": {"name": "valid"}}),
    ];
    let (tools, _) = translate_openai_tools_to_gemini(Some(&tools_in), None);
    assert_eq!(
        tools.expect("expected tools")[0].function_declarations[0].name,
        "valid"
    );

    let tools_in = vec![
        json!({"type": "function", "function": {"description": "no name"}}),
        json!("not-an-object"),
    ];
    let (tools, cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
    assert!(tools.is_none() && cfg.is_none());
}

#[test]
fn test_no_tools_means_no_tool_config() {
    let (tools, cfg) = translate_openai_tools_to_gemini(None, None);
    assert!(tools.is_none() && cfg.is_none());

    let req = openproxy_types::OpenAIRequest::default();
    let gemini_req = openai_to_gemini(&req, &[]);
    let serialized = serde_json::to_string(&gemini_req).unwrap();
    assert!(!serialized.contains("\"tools\"") && !serialized.contains("\"toolConfig\""));
}

#[test]
fn test_non_object_parameters_skips_tool() {
    let tools_in = vec![
        json!({"type": "function", "function": {"name": "bad", "parameters": "not-an-object"}}),
        json!({"type": "function", "function": {"name": "ok", "parameters": {"type": "object", "properties": {}}}}),
    ];
    let (tools, _) = translate_openai_tools_to_gemini(Some(&tools_in), None);
    assert_eq!(
        tools.expect("expected tools")[0].function_declarations[0].name,
        "ok"
    );

    let (tools_bad, cfg_bad) = translate_openai_tools_to_gemini(
        Some(&[json!({"type": "function", "function": {"name": "only_bad", "parameters": null}})]),
        None,
    );
    assert!(tools_bad.is_none() && cfg_bad.is_none());
}

#[test]
fn test_missing_parameters_is_not_a_warning() {
    let tools_in = vec![json!({
        "type": "function",
        "function": {"name": "no_params"}
    })];
    let (tools, _cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
    let tools = tools.expect("expected tools");
    assert_eq!(tools[0].function_declarations.len(), 1);
    assert_eq!(tools[0].function_declarations[0].name, "no_params");
    assert!(tools[0].function_declarations[0].parameters.is_none());
}

#[test]
fn test_serialize_gemini_request_happy_path() {
    let req = openproxy_types::OpenAIRequest::default();
    let messages = vec![openproxy_types::OpenAIMessage {
        role: "user".to_string(),
        content: Some(json!("hello")),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: serde_json::Map::new(),
    }];
    let bytes = serialize_gemini_request(&req, &messages).expect("happy path must serialize");
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");
    assert_eq!(parsed["contents"][0]["role"], "user");
    assert_eq!(parsed["contents"][0]["parts"][0]["text"], "hello");
}

#[test]
fn test_deserialize_gemini_response_happy_path() {
    let body = json!({
        "candidates": [{
            "content": {"parts": [{"text": "hi there"}], "role": "model"},
            "finish_reason": "STOP"
        }],
        "usage_metadata": {
            "prompt_token_count": 5,
            "candidates_token_count": 3,
            "total_token_count": 8
        }
    });
    let resp = deserialize_gemini_response(&body).expect("happy path must deserialize");
    assert_eq!(
        resp.choices[0].message.content.as_ref(),
        Some(&json!("hi there"))
    );
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
    let usage = resp.usage.expect("usage must be mapped");
    assert_eq!(usage.prompt_tokens, 5);
    assert_eq!(usage.completion_tokens, 3);
    assert_eq!(usage.total_tokens, 8);
}

#[test]
fn test_deserialize_gemini_response_function_call() {
    let body = json!({
        "candidates": [{
            "content": {
                "parts": [{
                    "functionCall": {
                        "name": "get_weather",
                        "args": {"city": "Tokyo"}
                    }
                }],
                "role": "model"
            },
            "finish_reason": "STOP"
        }],
        "usage_metadata": {
            "prompt_token_count": 10,
            "candidates_token_count": 8,
            "total_token_count": 18
        }
    });
    let resp = deserialize_gemini_response(&body).expect("must deserialize function call");
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("tool_calls"));
    let tool_calls = resp.choices[0]
        .message
        .tool_calls
        .as_ref()
        .expect("tool_calls present");
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0]["type"], "function");
    assert_eq!(tool_calls[0]["function"]["name"], "get_weather");
    assert_eq!(
        tool_calls[0]["function"]["arguments"],
        r#"{"city":"Tokyo"}"#
    );
}

#[test]
fn test_serialize_gemini_request_assistant_tool_calls_and_tool_response() {
    let req = openproxy_types::OpenAIRequest {
        model: "gemini-pro".into(),
        messages: vec![],
        stream: false,
        ..Default::default()
    };
    let messages = vec![
        openproxy_types::OpenAIMessage {
            role: "assistant".to_string(),
            content: None,
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![json!({
                "id": "call_weather_1",
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "arguments": "{\"city\":\"Madrid\"}"
                }
            })]),
            extra: serde_json::Map::new(),
        },
        openproxy_types::OpenAIMessage {
            role: "tool".to_string(),
            content: Some(json!("{\"temp\": 25}")),
            name: Some("get_weather".to_string()),
            tool_call_id: Some("call_weather_1".to_string()),
            tool_calls: None,
            extra: serde_json::Map::new(),
        },
    ];

    let bytes = serialize_gemini_request(&req, &messages).expect("must serialize");
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");

    let contents = parsed["contents"].as_array().expect("contents is array");
    assert_eq!(contents.len(), 2);

    // First message: assistant -> model with functionCall
    assert_eq!(contents[0]["role"], "model");
    let fc = &contents[0]["parts"][0]["functionCall"];
    assert_eq!(fc["name"], "get_weather");
    assert_eq!(fc["args"]["city"], "Madrid");

    // Second message: tool -> function with functionResponse
    assert_eq!(contents[1]["role"], "function");
    let fr = &contents[1]["parts"][0]["functionResponse"];
    assert_eq!(fr["name"], "get_weather");
    assert_eq!(fr["response"]["name"], "get_weather");
    assert_eq!(fr["response"]["content"]["temp"], 25);
}
