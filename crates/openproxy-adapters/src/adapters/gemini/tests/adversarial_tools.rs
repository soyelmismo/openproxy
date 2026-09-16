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
