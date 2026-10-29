use super::*;
use openproxy_types::{ModelId, TargetFormat};
use serde_json::json;

#[test]
fn test_claude_opus_4_6_thinking_no_sentinel_thought_signatures() {
    let mut contents = json!([
        {
            "role": "model",
            "parts": [
                {
                    "thought": true,
                    "text": "reasoning...",
                    "thought_signature": "fake-sig"
                },
                {
                    "functionCall": {
                        "name": "get_file",
                        "args": {}
                    }
                }
            ]
        }
    ]);

    tokens::inject_sentinel_thought_signatures(&mut contents, "claude-opus-4-6-thinking");
    let parts = contents[0]["parts"].as_array().expect("parts array");

    // Must NOT have thoughtSignature injected for Claude
    for part in parts {
        assert!(
            part.get("thoughtSignature").is_none(),
            "Claude must never receive Gemini thoughtSignature"
        );
        assert!(
            part.get("thought_signature").is_none(),
            "snake_case thought_signature must be purged"
        );
    }

    // A short fake signature is not real: the thought part is downgraded to
    // plain text so the Claude bridge never sees an unsigned thinking block.
    assert_eq!(
        parts[0].get("thought"),
        None,
        "unsigned thought part must lose the thought flag"
    );
    assert_eq!(parts[0]["text"], "reasoning...");
}

#[test]
fn test_claude_thinking_reasoning_echo_downgraded_to_text() {
    // Regression: assistant reasoning echoes (reasoning_content) reach the
    // Antigravity adapter as thought parts without any signature. The Claude
    // bridge turns them into thinking blocks and the upstream rejects the
    // request with `messages.N.content.0.thinking.signature: Field required`.
    let mut contents = json!([
        {
            "role": "model",
            "parts": [
                { "thought": true, "text": "el usuario saluda, respondo breve" },
                { "text": "hola" }
            ]
        }
    ]);

    tokens::inject_sentinel_thought_signatures(&mut contents, "claude-opus-4-6-thinking");
    let parts = contents[0]["parts"].as_array().expect("parts array");

    assert_eq!(parts.len(), 2);
    for part in parts {
        assert!(
            part.get("thought").is_none(),
            "no thought flag may survive without a real signature"
        );
    }
    assert_eq!(parts[0]["text"], "el usuario saluda, respondo breve");
    assert_eq!(parts[1]["text"], "hola");
}

#[test]
fn test_claude_thinking_placeholder_thought_dropped_keeping_tool_call() {
    let mut contents = json!([
        {
            "role": "model",
            "parts": [
                { "thought": true, "text": "..." },
                {
                    "functionCall": {
                        "name": "get_file",
                        "args": {}
                    }
                }
            ]
        }
    ]);

    tokens::inject_sentinel_thought_signatures(&mut contents, "claude-opus-4-6-thinking");
    let parts = contents[0]["parts"].as_array().expect("parts array");

    assert_eq!(
        parts.len(),
        1,
        "placeholder reasoning carries no information"
    );
    assert!(parts[0].get("functionCall").is_some());
    assert!(parts[0].get("thoughtSignature").is_none());
}

#[test]
fn test_claude_thinking_all_parts_dropped_gets_placeholder_text() {
    let mut contents = json!([
        {
            "role": "model",
            "parts": [
                { "thought": true, "text": "·" }
            ]
        }
    ]);

    tokens::inject_sentinel_thought_signatures(&mut contents, "claude-opus-4-6-thinking");
    let parts = contents[0]["parts"].as_array().expect("parts array");

    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0]["text"], "...");
    assert!(parts[0].get("thought").is_none());
}

#[test]
fn test_claude_thinking_sentinel_signature_is_not_real() {
    let mut contents = json!([
        {
            "role": "model",
            "parts": [
                { "thought": true, "text": "reasoning", "thoughtSignature": "skip_thought_signature_validator" }
            ]
        }
    ]);

    tokens::inject_sentinel_thought_signatures(&mut contents, "claude-opus-4-6-thinking");
    let parts = contents[0]["parts"].as_array().expect("parts array");

    // The Gemini sentinel is rejected by Claude upstreams (400 Invalid
    // signature): it must not be treated as a real signature.
    assert!(parts[0].get("thought").is_none());
    assert!(parts[0].get("thoughtSignature").is_none());
    assert_eq!(parts[0]["text"], "reasoning");
}

#[test]
fn test_claude_thinking_real_signature_moves_to_anchor_part() {
    let real_sig = "EqMOCqAOAWkUfRO0cKuvrvE9Zsa+uKZHZp2d25mCpd2a2h7oN+QkyxGc5MVWZg6/rpw73";
    let mut contents = json!([
        {
            "role": "model",
            "parts": [
                { "thought": true, "text": "deep reasoning", "thoughtSignature": real_sig },
                { "text": "answer" },
                {
                    "functionCall": {
                        "name": "get_file",
                        "args": {}
                    }
                }
            ]
        }
    ]);

    tokens::inject_sentinel_thought_signatures(&mut contents, "claude-opus-4-6-thinking");
    let parts = contents[0]["parts"].as_array().expect("parts array");

    // The thinking block keeps its flag and stays signature-free...
    assert_eq!(parts[0].get("thought"), Some(&json!(true)));
    assert!(parts[0].get("thoughtSignature").is_none());
    // ...the signature rides on the anchor part (first non-thought part).
    assert_eq!(parts[1]["thoughtSignature"], json!(real_sig));
    // Function calls never carry signatures for Claude.
    assert!(parts[2].get("thoughtSignature").is_none());
}

#[test]
fn test_wrap_request_body_claude_sanitizes_role_function_and_correlates_ids() {
    let adapter = AntigravityAdapter::new();
    let body_json = json!({
        "contents": [
            {
                "role": "user",
                "parts": [{"text": "call"}]
            },
            {
                "role": "model",
                "parts": [
                    {
                        "functionCall": {
                            "name": "calc",
                            "args": {}
                        }
                    }
                ]
            },
            {
                "role": "function",
                "parts": [
                    {
                        "functionResponse": {
                            "name": "calc",
                            "response": {"result": 42}
                        }
                    }
                ]
            }
        ],
        "generationConfig": {
            "maxOutputTokens": 2048,
            "thinkingConfig": {
                "thinkingBudget": 500
            }
        }
    });

    let raw_bytes = bytes::Bytes::from(serde_json::to_vec(&body_json).unwrap());
    let target = openproxy_types::context::ResolvedTarget {
        target: openproxy_types::combos::ComboTarget {
            id: openproxy_types::ComboTargetId(1),
            combo_id: openproxy_types::ComboId(1),
            provider_id: openproxy_types::ProviderId::new("antigravity"),
            priority_order: 1,
            ..Default::default()
        },
        model: openproxy_types::Model {
            row_id: openproxy_types::ModelRowId(1),
            provider_id: openproxy_types::ProviderId::new("antigravity"),
            model_id: openproxy_types::ModelId::new("claude-opus-4-6-thinking"),
            target_format: openproxy_types::TargetFormat::Gemini,
            discovered_at: openproxy_types::now_unix_secs_str().into_boxed_str(),
            ..Default::default()
        },
        api_key: "k".to_string(),
        ..Default::default()
    };

    let wrapped = adapter
        .wrap_request_body(
            raw_bytes,
            TargetFormat::Gemini,
            &ModelId::new("claude-opus-4-6-thinking"),
            &target,
        )
        .expect("wrap_request_body should succeed");

    let val: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();
    let req = &val["request"];
    let contents = req["contents"].as_array().unwrap();

    // 1. Role "function" must be converted to "user"
    assert_eq!(contents[2]["role"], "user");

    // 2. Both functionCall and functionResponse must have matching IDs
    let fc_id = contents[1]["parts"][0]["functionCall"]["id"]
        .as_str()
        .unwrap();
    let fr_id = contents[2]["parts"][0]["functionResponse"]["id"]
        .as_str()
        .unwrap();
    assert!(!fc_id.is_empty());
    assert_eq!(fc_id, fr_id);

    // 3. thinkingBudget clamped to at least 1024, and maxOutputTokens has headroom (> 1024)
    let gen_cfg = &req["generationConfig"];
    assert_eq!(
        gen_cfg["thinkingConfig"]["thinkingBudget"].as_i64(),
        Some(1024)
    );
    assert!(gen_cfg["maxOutputTokens"].as_i64().unwrap() > 1024);

    // 4. functionResponse.response strictly normalized to { "output": 42 }
    let fr_resp = &contents[2]["parts"][0]["functionResponse"]["response"];
    assert_eq!(fr_resp["output"], 42);

    // 5. Envelope has no enabledCreditTypes
    assert_eq!(val.get("enabledCreditTypes"), None);
}

#[test]
fn test_wrap_request_body_gemini_sanitizes_role_function_to_model_and_formats_output() {
    let adapter = AntigravityAdapter::new();
    let body_json = json!({
        "contents": [
            {
                "role": "user",
                "parts": [{"text": "run"}]
            },
            {
                "role": "model",
                "parts": [
                    {
                        "functionCall": {
                            "name": "shell",
                            "args": {"cmd": "echo hi"}
                        }
                    }
                ]
            },
            {
                "role": "function",
                "parts": [
                    {
                        "functionResponse": {
                            "name": "shell",
                            "response": {
                                "name": "shell",
                                "content": "hello world"
                            }
                        }
                    }
                ]
            }
        ],
        "tools": [
            {
                "functionDeclarations": [
                    {
                        "name": "shell",
                        "description": "run shell",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "cmd": {"type": "string"}
                            }
                        }
                    },
                    {
                        "name": "read_file",
                        "description": "read a file",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string"}
                            }
                        }
                    }
                ]
            }
        ]
    });

    let raw_bytes = bytes::Bytes::from(serde_json::to_vec(&body_json).unwrap());
    let target = openproxy_types::context::ResolvedTarget {
        target: openproxy_types::combos::ComboTarget {
            id: openproxy_types::ComboTargetId(1),
            combo_id: openproxy_types::ComboId(1),
            provider_id: openproxy_types::ProviderId::new("antigravity"),
            priority_order: 1,
            ..Default::default()
        },
        model: openproxy_types::Model {
            row_id: openproxy_types::ModelRowId(1),
            provider_id: openproxy_types::ProviderId::new("antigravity"),
            model_id: openproxy_types::ModelId::new("gemini-3.8-flash-high"),
            target_format: openproxy_types::TargetFormat::Gemini,
            discovered_at: openproxy_types::now_unix_secs_str().into_boxed_str(),
            ..Default::default()
        },
        api_key: "k".to_string(),
        ..Default::default()
    };

    let wrapped = adapter
        .wrap_request_body(
            raw_bytes,
            TargetFormat::Gemini,
            &ModelId::new("gemini-3.8-flash-high"),
            &target,
        )
        .expect("wrap_request_body should succeed");

    let val: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();
    let req = &val["request"];
    let contents = req["contents"].as_array().unwrap();

    // 1. Role "function" must be converted to "model" for Gemini
    assert_eq!(contents[2]["role"], "model");

    // 2. Both functionCall and functionResponse must have matching call_... IDs
    let fc_part = contents[1]["parts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p.get("functionCall").is_some())
        .expect("functionCall part must exist");
    let fc_id = fc_part["functionCall"]["id"].as_str().unwrap();
    let fr = &contents[2]["parts"][0]["functionResponse"];
    let fr_id = fr["id"].as_str().unwrap();
    assert!(fc_id.starts_with("call_"));
    assert_eq!(fc_id, fr_id);

    // 3. functionResponse.response strictly formatted to { "output": "hello world" }
    assert_eq!(fr["response"]["output"], "hello world");

    // 4. Tools expanded to individual declarations with uppercase types
    let tools = req["tools"].as_array().unwrap();
    assert_eq!(
        tools.len(),
        2,
        "tools must be split into single-declaration objects"
    );
    let t0_params = &tools[0]["functionDeclarations"][0]["parameters"];
    assert_eq!(t0_params["type"], "OBJECT");
    assert_eq!(t0_params["properties"]["cmd"]["type"], "STRING");

    // 5. Envelope has no enabledCreditTypes
    assert_eq!(val.get("enabledCreditTypes"), None);
}
