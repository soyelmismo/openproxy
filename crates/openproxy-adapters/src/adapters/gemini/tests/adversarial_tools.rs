use super::super::*;
use serde_json::json;

// --- Boundary / Type Violation tests for `parameters` ---

#[test]
fn adv_parameters_invalid_types_skip_tool() {
    for bad in [
        json!("this is not a schema"),
        json!(42),
        json!(["type", "string"]),
        json!(true),
        json!(null),
    ] {
        let tools_in =
            vec![json!({"type": "function", "function": {"name": "bad", "parameters": bad}})];
        let (tools, _) = translate_openai_tools_to_gemini(Some(&tools_in), None);
        assert!(
            tools.is_none(),
            "bad parameters {bad:?} must cause tool skip"
        );
    }
}

#[test]
fn adv_tool_name_edge_cases() {
    for (name, exp) in [("", ""), ("   ", "   ")] {
        let tools_in = vec![json!({"type": "function", "function": {"name": name}})];
        let (tools, _) = translate_openai_tools_to_gemini(Some(&tools_in), None);
        assert_eq!(tools.unwrap()[0].function_declarations[0].name, exp);
    }
    for bad in [json!(42), json!(true)] {
        let (t1, _) = translate_openai_tools_to_gemini(
            Some(&[json!({"type": "function", "function": bad})]),
            None,
        );
        assert!(t1.is_none());
        let (t2, _) = translate_openai_tools_to_gemini(
            Some(&[json!({"type": "function", "function": {"name": bad}})]),
            None,
        );
        assert!(t2.is_none());
    }
}

// --- Description edge cases ---

#[test]
fn adv_description_very_long_preserved() {
    let long_desc = "x".repeat(1_000_000);
    let tools_in = vec![json!({
        "type": "function",
        "function": {"name": "big", "description": long_desc}
    })];
    let (tools, _cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
    let tools = tools.expect("expected tools");
    let desc = tools[0].function_declarations[0]
        .description
        .as_ref()
        .unwrap();
    assert_eq!(desc.len(), 1_000_000);
}

// --- tool_choice edge cases ---

#[test]
fn adv_tool_choice_fallbacks_to_auto() {
    let dummy_tool = vec![json!({"type":"function","function":{"name":"x"}})];
    for tc in [
        json!("banana"),
        json!(42),
        json!(["auto"]),
        json!(true),
        json!({"type": "function"}),
        json!({"type": "function", "function": {"description": "no name"}}),
    ] {
        let (_, cfg) = translate_openai_tools_to_gemini(Some(&dummy_tool), Some(&tc));
        assert!(matches!(
            cfg.unwrap().function_calling_config.mode,
            GeminiFunctionCallingMode::Auto
        ));
    }
}

#[test]
fn adv_tool_choice_object_huge_function_name_preserved() {
    let dummy_tool = vec![json!({"type":"function","function":{"name":"x"}})];
    let big_name = "a".repeat(10_000);
    let tc = json!({"type": "function", "function": {"name": big_name}});
    let (_, cfg) = translate_openai_tools_to_gemini(Some(&dummy_tool), Some(&tc));
    let cfg = cfg.unwrap();
    assert!(matches!(
        cfg.function_calling_config.mode,
        GeminiFunctionCallingMode::Any
    ));
    let names = cfg.function_calling_config.allowed_function_names.unwrap();
    assert_eq!(names[0].len(), 10_000);
}

// --- Duplicate tools ---

#[test]
fn adv_duplicate_tool_names_both_preserved() {
    let tools_in = vec![
        json!({"type":"function","function":{"name":"dup","description":"first"}}),
        json!({"type":"function","function":{"name":"dup","description":"second"}}),
    ];
    let (tools, _cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
    let tools = tools.expect("expected tools");
    assert_eq!(tools[0].function_declarations.len(), 2);
    assert_eq!(tools[0].function_declarations[0].name, "dup");
    assert_eq!(tools[0].function_declarations[1].name, "dup");
}

// --- Enum in parameters preserved ---

#[test]
fn adv_enum_in_parameters_preserved() {
    let tools_in = vec![json!({
        "type": "function",
        "function": {
            "name": "with_enum",
            "parameters": {
                "type": "object",
                "properties": {
                    "color": {"type": "string", "enum": ["red", "green", "blue"]}
                }
            }
        }
    })];
    let (tools, _cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
    let tools = tools.expect("expected tools");
    let params = tools[0].function_declarations[0]
        .parameters
        .as_ref()
        .unwrap();
    assert_eq!(params["properties"]["color"]["enum"][0], "red");
    assert_eq!(params["properties"]["color"]["enum"][1], "green");
    assert_eq!(params["properties"]["color"]["enum"][2], "blue");
}

// --- 1000 tools: all should be mapped ---

#[test]
fn adv_1000_tools_all_mapped() {
    let tools_in: Vec<serde_json::Value> = (0..1000)
        .map(|i| {
            json!({
                "type": "function",
                "function": {"name": format!("tool_{i:04}")}
            })
        })
        .collect();
    let (tools, cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
    let tools = tools.expect("expected tools for 1000 tools");
    assert_eq!(tools[0].function_declarations.len(), 1000);
    assert_eq!(tools[0].function_declarations[0].name, "tool_0000");
    assert_eq!(tools[0].function_declarations[999].name, "tool_0999");
    let cfg = cfg.unwrap();
    assert!(matches!(
        cfg.function_calling_config.mode,
        GeminiFunctionCallingMode::Auto
    ));
}

// --- Non-object tool values ---

#[test]
fn adv_non_object_tool_skipped() {
    for bad in [json!("not-an-object"), json!(42)] {
        let (tools, _) = translate_openai_tools_to_gemini(Some(&[bad]), None);
        assert!(tools.is_none());
    }
}

// --- Parameters null vs missing: different behavior ---

#[test]
fn adv_parameters_null_vs_absent() {
    let tools_null = vec![json!({
        "type": "function",
        "function": {"name": "with_null", "parameters": null}
    })];
    let (tools, _) = translate_openai_tools_to_gemini(Some(&tools_null), None);
    assert!(tools.is_none(), "null parameters must skip tool");

    let tools_absent = vec![json!({
        "type": "function",
        "function": {"name": "without_params"}
    })];
    let (tools, _) = translate_openai_tools_to_gemini(Some(&tools_absent), None);
    let tools = tools.expect("absent parameters should keep tool");
    assert!(tools[0].function_declarations[0].parameters.is_none());
}

// --- tool_choice None + tools present → config emitted ---

#[test]
fn adv_tools_present_tool_choice_none_emits_auto_config() {
    let tools_in = vec![json!({
        "type": "function",
        "function": {"name": "x"}
    })];
    let (_, cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
    let cfg = cfg.expect("tool_config should be Some when tools present");
    assert!(
        matches!(
            cfg.function_calling_config.mode,
            GeminiFunctionCallingMode::Auto
        ),
        "no explicit tool_choice with tools → Auto"
    );
}

// --- tool_choice None + tools=None → both None ---

#[test]
fn adv_no_tools_no_tool_choice_both_none() {
    let (tools, cfg) = translate_openai_tools_to_gemini(None, None);
    assert!(tools.is_none());
    assert!(cfg.is_none());
}

// --- Circular $ref in parameters: should not panic (depth limit) ---

#[test]
fn adv_circular_ref_in_parameters_no_panic() {
    let tools_in = vec![json!({
        "type": "function",
        "function": {
            "name": "circular",
            "parameters": {
                "$defs": {
                    "Node": {
                        "type": "object",
                        "properties": {
                            "child": {"$ref": "#/$defs/Node"}
                        }
                    }
                },
                "type": "object",
                "properties": {
                    "root": {"$ref": "#/$defs/Node"}
                }
            }
        }
    })];
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = translate_openai_tools_to_gemini(Some(&tools_in), None);
    }));
    assert!(
        result.is_ok(),
        "circular $ref must not cause stack overflow panic"
    );
}

// --- Serialized output omits tools/toolConfig when None ---

#[test]
fn adv_serialized_output_omits_tools_when_none() {
    let gemini_req = openai_to_gemini(&openproxy_types::OpenAIRequest::default(), &[]);
    let json_str = serde_json::to_string(&gemini_req).unwrap();
    assert!(
        !json_str.contains("\"tools\""),
        "serialized must not contain tools field when None"
    );
    assert!(
        !json_str.contains("\"toolConfig\""),
        "serialized must not contain toolConfig field when None"
    );
}

// --- Adversarial: Tool calls with empty arguments ---

#[test]
fn adv_tool_calls_empty_arguments() {
    // Case 1: args is empty object {}
    let body_empty_obj = json!({
        "candidates": [{
            "content": {
                "parts": [{
                    "functionCall": {
                        "name": "no_args_func",
                        "args": {}
                    }
                }],
                "role": "model"
            },
            "finish_reason": "STOP"
        }]
    });
    let resp = deserialize_gemini_response(&body_empty_obj).expect("deserialize empty obj");
    let tc = &resp.choices[0].message.tool_calls.as_ref().unwrap()[0];
    assert_eq!(tc["function"]["name"], "no_args_func");
    assert_eq!(tc["function"]["arguments"], "{}");

    // Case 2: args omitted entirely from JSON
    let body_missing_args = json!({
        "candidates": [{
            "content": {
                "parts": [{
                    "functionCall": {
                        "name": "omitted_args_func"
                    }
                }],
                "role": "model"
            },
            "finish_reason": "STOP"
        }]
    });
    let resp2 = deserialize_gemini_response(&body_missing_args).expect("deserialize missing args");
    let tc2 = &resp2.choices[0].message.tool_calls.as_ref().unwrap()[0];
    assert_eq!(tc2["function"]["name"], "omitted_args_func");
    assert_eq!(tc2["function"]["arguments"], "{}");

    // Case 2b: args is explicit null in JSON
    let body_null_args = json!({
        "candidates": [{
            "content": {
                "parts": [{
                    "functionCall": {
                        "name": "null_args_func",
                        "args": null
                    }
                }],
                "role": "model"
            },
            "finish_reason": "STOP"
        }]
    });
    let resp_null = deserialize_gemini_response(&body_null_args).expect("deserialize null args");
    let tc_null = &resp_null.choices[0].message.tool_calls.as_ref().unwrap()[0];
    assert_eq!(tc_null["function"]["name"], "null_args_func");
    assert_eq!(tc_null["function"]["arguments"], "{}");

    // Case 3: OpenAI request with assistant message having empty arguments
    for empty_args in ["{}", "", "null"] {
        let req = openproxy_types::OpenAIRequest::default();
        let messages = vec![openproxy_types::OpenAIMessage {
            role: "assistant".to_string(),
            content: None,
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![json!({
                "id": "call_1",
                "type": "function",
                "function": {
                    "name": "test_empty",
                    "arguments": empty_args
                }
            })]),
            extra: serde_json::Map::new(),
        }];
        let gemini_req = openai_to_gemini(&req, &messages);
        let part = &gemini_req.contents[0].parts[0];
        let fc = part.function_call.as_ref().expect("function_call present");
        assert_eq!(fc.name, "test_empty");
        assert!(fc.args.is_object() || fc.args.is_null());
    }
}

// --- Adversarial: Multiple tool calls ---

#[test]
fn adv_multiple_tool_calls() {
    let body = json!({
        "candidates": [{
            "content": {
                "parts": [
                    {
                        "functionCall": {
                            "name": "get_weather",
                            "args": {"city": "Paris"},
                            "id": "call_paris"
                        }
                    },
                    {
                        "functionCall": {
                            "name": "get_time",
                            "args": {"timezone": "CET"},
                            "id": "call_cet"
                        }
                    },
                    {
                        "functionCall": {
                            "name": "send_notification",
                            "args": {"user": "alice", "msg": "hi"}
                        }
                    }
                ],
                "role": "model"
            },
            "finish_reason": "STOP"
        }]
    });
    let resp = deserialize_gemini_response(&body).expect("deserialize multiple function calls");
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("tool_calls"));
    assert!(resp.choices[0].message.content.is_none());
    let tcs = resp.choices[0].message.tool_calls.as_ref().expect("tool_calls present");
    assert_eq!(tcs.len(), 3);
    assert_eq!(tcs[0]["id"], "call_paris");
    assert_eq!(tcs[0]["function"]["name"], "get_weather");
    assert_eq!(tcs[1]["id"], "call_cet");
    assert_eq!(tcs[1]["function"]["name"], "get_time");
    assert_eq!(tcs[2]["function"]["name"], "send_notification");
    assert!(tcs[2]["id"].as_str().unwrap().starts_with("call_gemini_"));

    // Request with assistant message having 3 tool calls
    let req = openproxy_types::OpenAIRequest::default();
    let messages = vec![openproxy_types::OpenAIMessage {
        role: "assistant".to_string(),
        content: None,
        name: None,
        tool_call_id: None,
        tool_calls: Some(vec![
            json!({"id": "c1", "type": "function", "function": {"name": "f1", "arguments": "{\"a\": 1}"}}),
            json!({"id": "c2", "type": "function", "function": {"name": "f2", "arguments": "{\"b\": 2}"}}),
            json!({"id": "c3", "type": "function", "function": {"name": "f3", "arguments": "{\"c\": 3}"}}),
        ]),
        extra: serde_json::Map::new(),
    }];
    let gemini_req = openai_to_gemini(&req, &messages);
    assert_eq!(gemini_req.contents[0].parts.len(), 3);
    assert_eq!(gemini_req.contents[0].parts[0].function_call.as_ref().unwrap().name, "f1");
    assert_eq!(gemini_req.contents[0].parts[1].function_call.as_ref().unwrap().name, "f2");
    assert_eq!(gemini_req.contents[0].parts[2].function_call.as_ref().unwrap().name, "f3");
}

// --- Adversarial: Tool responses with structured JSON content ---

#[test]
fn adv_tool_responses_structured_json_content() {
    let req = openproxy_types::OpenAIRequest::default();

    // 1. Structured JSON object with nested hierarchy
    let nested_obj = json!({
        "status": "success",
        "data": {
            "temperature": 21.5,
            "humidity": 65,
            "forecast": ["sunny", "cloudy"],
            "metadata": {
                "sensor_id": 42,
                "calibrated": true
            }
        }
    });
    let messages_obj = vec![openproxy_types::OpenAIMessage {
        role: "tool".to_string(),
        content: Some(nested_obj),
        name: Some("sensor_read".to_string()),
        tool_call_id: Some("call_sensor_1".to_string()),
        tool_calls: None,
        extra: serde_json::Map::new(),
    }];
    let gemini_req = openai_to_gemini(&req, &messages_obj);
    let part = &gemini_req.contents[0].parts[0];
    let fr = part.function_response.as_ref().expect("function_response present");
    assert_eq!(fr.name, "sensor_read");
    // Object content should be preserved directly without {"output": ...} wrapping
    assert_eq!(fr.response["content"]["status"], "success");
    assert_eq!(fr.response["content"]["data"]["temperature"], 21.5);
    assert_eq!(fr.response["content"]["data"]["metadata"]["calibrated"], true);

    // 2. Structured JSON array (as standard OpenAI wire-format string)
    let array_val = json!(["item1", "item2", 123]);
    let messages_arr = vec![openproxy_types::OpenAIMessage {
        role: "tool".to_string(),
        content: Some(json!(serde_json::to_string(&array_val).unwrap())),
        name: Some("list_items".to_string()),
        tool_call_id: Some("call_list_1".to_string()),
        tool_calls: None,
        extra: serde_json::Map::new(),
    }];
    let gemini_req_arr = openai_to_gemini(&req, &messages_arr);
    let fr_arr = gemini_req_arr.contents[0].parts[0].function_response.as_ref().unwrap();
    // Non-object JSON is wrapped in {"output": ...}
    assert_eq!(fr_arr.response["content"]["output"], array_val);

    // 2b. Structured JSON array directly as Value::Array in content
    let messages_arr_direct = vec![openproxy_types::OpenAIMessage {
        role: "tool".to_string(),
        content: Some(array_val.clone()),
        name: Some("list_items".to_string()),
        tool_call_id: Some("call_list_1".to_string()),
        tool_calls: None,
        extra: serde_json::Map::new(),
    }];
    let gemini_req_arr_direct = openai_to_gemini(&req, &messages_arr_direct);
    let fr_arr_direct = gemini_req_arr_direct.contents[0].parts[0].function_response.as_ref().unwrap();
    assert_eq!(fr_arr_direct.response["content"]["output"], array_val);

    // 3. Primitive values (number, boolean, null)
    for (prim, exp) in [
        (json!(42), json!(42)),
        (json!(true), json!(true)),
        (json!(null), json!("")),
    ] {
        let msgs = vec![openproxy_types::OpenAIMessage {
            role: "tool".to_string(),
            content: Some(prim),
            name: Some("get_primitive".to_string()),
            tool_call_id: Some("call_prim".to_string()),
            tool_calls: None,
            extra: serde_json::Map::new(),
        }];
        let g_req = openai_to_gemini(&req, &msgs);
        let fr_prim = g_req.contents[0].parts[0].function_response.as_ref().unwrap();
        assert_eq!(fr_prim.response["content"]["output"], exp);
    }

    // 4. Verify serialized Gemini request wire format
    let bytes = serialize_gemini_request(&req, &messages_obj).expect("serialize structured tool response");
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON wire format");
    assert_eq!(parsed["contents"][0]["role"], "function");
    assert_eq!(
        parsed["contents"][0]["parts"][0]["functionResponse"]["response"]["content"]["status"],
        "success"
    );
}

