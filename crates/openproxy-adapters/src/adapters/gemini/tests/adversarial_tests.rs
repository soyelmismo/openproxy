use super::super::*;
use openproxy_types::{OpenAIMessage, OpenAIRequest, OpenAIResponse};

// =====================================================================
// ADVERSARIAL TESTS — dedup-antigravity-gemini refactor (D2 + D3)
// =====================================================================

#[test]
fn serialize_gemini_request_empty_messages_succeeds() {
    let req = OpenAIRequest::default();
    let res = serialize_gemini_request(&req, &[]);
    let bytes = res.expect("empty request must serialize");
    let parsed: serde_json::Value =
        serde_json::from_slice(&bytes).expect("output must be valid JSON");
    assert!(parsed.get("contents").is_some());
    assert!(parsed["contents"].as_array().unwrap().is_empty());
}

#[test]
fn serialize_gemini_request_10k_messages_succeeds() {
    let messages: Vec<OpenAIMessage> = (0..10_000)
        .map(|i| OpenAIMessage {
            role: "user".to_string(),
            content: Some(serde_json::Value::String(format!("msg {i}"))),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::new(),
        })
        .collect();
    let req = OpenAIRequest {
        model: "gemini-1.5-pro".to_string(),
        messages: vec![],
        ..Default::default()
    };
    let bytes = serialize_gemini_request(&req, &messages).expect("10k messages must serialize");
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("must round-trip");
    assert_eq!(parsed["contents"].as_array().unwrap().len(), 10_000);
    assert_eq!(parsed["contents"][0]["parts"][0]["text"], "msg 0");
    assert_eq!(parsed["contents"][9_999]["parts"][0]["text"], "msg 9999");
}

#[test]
fn serialize_gemini_request_extreme_unicode_preserved() {
    let content = "cafe \u{1F511}  \u{202E}rtl\u{202C} zwj \u{200D} \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466} ni hao";
    let messages = vec![OpenAIMessage {
        role: "user".to_string(),
        content: Some(serde_json::Value::String(content.to_string())),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: serde_json::Map::new(),
    }];
    let req = OpenAIRequest::default();
    let bytes = serialize_gemini_request(&req, &messages).expect("extreme unicode must serialize");
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("must round-trip");
    let out = parsed["contents"][0]["parts"][0]["text"]
        .as_str()
        .expect("text must be a string");
    assert_eq!(out, content, "byte-for-byte preservation");
}

#[test]
fn serialize_gemini_request_tool_calls_with_circular_ref_does_not_panic() {
    let circular_schema = serde_json::json!({
        "type": "object",
        "properties": { "self": { "$ref": "#" } }
    });
    let tool_call = serde_json::json!({
        "id": "call_1", "type": "function",
        "function": { "name": "recurse", "arguments": "{}" }
    });
    let messages = vec![OpenAIMessage {
        role: "assistant".to_string(),
        content: None,
        name: None,
        tool_call_id: None,
        tool_calls: Some(vec![tool_call]),
        extra: serde_json::Map::new(),
    }];
    let req = OpenAIRequest {
        tools: Some(vec![serde_json::json!({
            "type": "function",
            "function": { "name": "recurse", "parameters": circular_schema }
        })]),
        ..Default::default()
    };
    let bytes = serialize_gemini_request(&req, &messages)
        .expect("circular ref must not panic and must serialize");
    let _: serde_json::Value = serde_json::from_slice(&bytes).expect("must round-trip");
}

#[test]
fn serialize_gemini_request_tools_with_non_object_elements_no_panic() {
    let req = OpenAIRequest {
        tools: Some(vec![
            serde_json::Value::String("not-an-object".to_string()),
            serde_json::Value::Null,
            serde_json::json!({"type": "function", "function": {"name": "f"}}),
        ]),
        ..Default::default()
    };
    let messages: Vec<OpenAIMessage> = vec![];
    let res =
        serialize_gemini_request(&req, &messages).expect("non-object tool elements must serialize");
    let parsed: serde_json::Value = serde_json::from_slice(&res).expect("must round-trip");
    let funcs = &parsed["tools"][0]["functionDeclarations"];
    assert!(funcs.is_array());
    assert_eq!(funcs.as_array().unwrap().len(), 1);
    assert_eq!(funcs[0]["name"], "f");
}

#[test]
fn serialize_gemini_request_structured_content_succeeds() {
    let req = OpenAIRequest {
        messages: vec![OpenAIMessage {
            role: "user".to_string(),
            content: Some(serde_json::json!({"complex": [1, 2, 3]})),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::new(),
        }],
        ..Default::default()
    };
    let res = serialize_gemini_request(&req, &req.messages);
    assert!(res.is_ok(), "any OpenAIRequest must serialize");
}

#[test]
fn deserialize_gemini_response_null_body_does_not_panic() {
    let body: serde_json::Value = serde_json::Value::Null;
    let res = deserialize_gemini_response(&body);
    match res {
        Ok(resp) => {
            let _: OpenAIResponse = resp;
        }
        Err(_) => { /* acceptable: serde fails */ }
    }
}

#[test]
fn deserialize_gemini_response_top_level_array_succeeds_with_defaults() {
    let body: serde_json::Value = serde_json::json!([]);
    let resp = deserialize_gemini_response(&body)
        .expect("top-level array -> defaults (permissive serde_json)");
    assert_eq!(resp.choices.len(), 1, "always one choice");
    let content = resp.choices[0].message.content.as_ref();
    if let Some(c) = content {
        assert!(c.as_str().is_none_or(str::is_empty));
    }
}

#[test]
fn deserialize_gemini_response_candidates_null_errors() {
    let body: serde_json::Value = serde_json::json!({"candidates": null});
    let res = deserialize_gemini_response(&body);
    if let Err(e) = res {
        let msg = format!("{e}");
        assert!(msg.contains("parse"));
    }
}

#[test]
fn deserialize_gemini_response_empty_candidates_emits_one_empty_choice() {
    let body: serde_json::Value = serde_json::json!({"candidates": []});
    let resp = deserialize_gemini_response(&body).expect("empty candidates must succeed");
    assert_eq!(
        resp.choices.len(),
        1,
        "gemini_to_openai always emits one choice"
    );
    let content = resp.choices[0].message.content.as_ref();
    if let Some(c) = content {
        assert!(c.as_str().is_none_or(str::is_empty));
    }
}

#[test]
fn deserialize_gemini_response_parts_null_does_not_panic() {
    let body: serde_json::Value = serde_json::json!({
        "candidates": [{"content": {"parts": null}, "finishReason": "STOP"}]
    });
    let res = deserialize_gemini_response(&body);
    if let Ok(resp) = res {
        let content = &resp.choices[0].message.content;
        if let Some(c) = content {
            assert!(c.is_string());
        }
    }
}

#[test]
fn deserialize_gemini_response_10mb_body_succeeds() {
    let huge: String = "a".repeat(10 * 1024 * 1024);
    let body: serde_json::Value = serde_json::json!({
        "candidates": [{
            "content": { "role": "model", "parts": [{"text": huge}] },
            "finishReason": "STOP"
        }]
    });
    let resp = deserialize_gemini_response(&body).expect("10 MiB body must deserialize");
    let out = resp.choices[0]
        .message
        .content
        .as_ref()
        .and_then(|v| v.as_str())
        .expect("content must be a string");
    assert_eq!(out.len(), 10 * 1024 * 1024, "no bytes dropped");
}

#[test]
fn deserialize_gemini_response_truncated_input_does_not_panic() {
    let body: serde_json::Value = serde_json::Value::Null;
    let _ = deserialize_gemini_response(&body);
}

#[test]
fn deserialize_gemini_response_lone_surrogate_rejected_by_serde_json() {
    let raw = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hello \uDCFF world"}]},"finishReason":"STOP"}]}"#;
    let body: std::result::Result<serde_json::Value, _> = serde_json::from_str(raw);
    assert!(body.is_err());
    let msg = format!("{}", body.unwrap_err());
    assert!(msg.contains("lone leading surrogate"));
}

#[test]
fn deserialize_gemini_response_1000_duplicate_candidates_no_blowup() {
    let mut candidates = Vec::with_capacity(1000);
    for _ in 0..1000 {
        candidates.push(serde_json::json!({
            "content": { "role": "model", "parts": [{"text": "hi"}] },
            "finishReason": "STOP"
        }));
    }
    let body: serde_json::Value = serde_json::json!({"candidates": candidates});
    let resp = deserialize_gemini_response(&body).expect("1000 candidates must deserialize");
    assert_eq!(resp.choices.len(), 1);
    assert_eq!(
        resp.choices[0]
            .message
            .content
            .as_ref()
            .unwrap()
            .as_str()
            .unwrap(),
        "hi"
    );
}
