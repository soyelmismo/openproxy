use super::*;
use openproxy_adapters::adapters::ProviderAdapterEnum;
use openproxy_adapters::adapters::nvidia_nim::NvidiaNimAdapter;
use openproxy_types::models::Model;
use openproxy_types::{
    ModelId, ModelRowId, OpenAIMessage, OpenAIRequest, ProviderId, TargetFormat,
};
use std::sync::Arc;

fn test_req(openai_req: OpenAIRequest) -> PipelineRequest {
    PipelineRequest {
        request_id: openproxy_types::RequestId::new(),
        trace_id: openproxy_types::TraceId::new(),
        combo_id: openproxy_types::ComboId(1),
        openai_request: Arc::new(openai_req),
        client_disconnected: tokio::sync::watch::channel(None).1,
        stream_sink: None,
        api_key_id: None,
        combo_override: None,
        targets_override: None,
        request_headers: Default::default(),
        request_body_json: None,
        race_cancelled: false,
        race_cancel: None,
        endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
        compressed_messages: Arc::new(std::sync::OnceLock::new()),
        pii_session: Arc::new(parking_lot::Mutex::new(None)),
        compression_stats: Arc::new(parking_lot::Mutex::new(None)),
        proxy_override: None,
    }
}

fn test_model() -> Model {
    Model {
        row_id: ModelRowId(1),
        provider_id: ProviderId::new("nvidia-nim"),
        model_id: ModelId::new("deepseek-ai/deepseek-v4-flash-0731"),
        display_name: Some("test".into()),
        target_format: TargetFormat::Openai,
        discovered_at: "2026-01-01T00:00:00Z".into(),
        active: true,
        model_type: "chat".into(),
        ..Default::default()
    }
}

#[test]
fn responses_input_does_not_emit_legacy_item_type() {
    let user = OpenAIMessage {
        role: "user".into(),
        content: Some(Value::String("ping".into())),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: Default::default(),
    };
    let tool = OpenAIMessage {
        role: "tool".into(),
        content: Some(Value::String("pong".into())),
        name: None,
        tool_call_id: Some("call_1".into()),
        tool_calls: None,
        extra: Default::default(),
    };
    let input = messages_to_responses_input(&[&user, &tool]);
    let items = input.as_array().expect("input array");
    assert_eq!(items[0].get("type"), None);
    assert_eq!(
        items[1].get("type").and_then(Value::as_str),
        Some("function_call_output")
    );
}

#[test]
fn test_responses_input_assistant_tool_calls_omits_empty_text() {
    let assistant = OpenAIMessage {
        role: "assistant".into(),
        content: None,
        name: None,
        tool_call_id: None,
        tool_calls: Some(vec![
            json!({ "id": "call_123", "type": "function", "function": { "name": "web_search", "arguments": "{\"q\":\"test\"}" } }),
        ]),
        extra: Default::default(),
    };
    let input = messages_to_responses_input(&[&assistant]);
    let items = input.as_array().expect("input array");
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].get("type").and_then(Value::as_str),
        Some("function_call")
    );
    assert_eq!(
        items[0].get("call_id").and_then(Value::as_str),
        Some("call_123")
    );
}

#[test]
fn test_responses_input_assistant_commentary_and_tool_calls() {
    let assistant = OpenAIMessage {
        role: "assistant".into(),
        content: Some(Value::String("Voy a buscar.".into())),
        name: None,
        tool_call_id: None,
        tool_calls: Some(vec![
            json!({ "id": "call_123", "type": "function", "function": { "name": "web_search", "arguments": "{\"q\":\"test\"}" } }),
        ]),
        extra: Default::default(),
    };
    let input = messages_to_responses_input(&[&assistant]);
    let items = input.as_array().expect("input array");
    assert_eq!(items.len(), 2);
    assert_eq!(
        items[0].get("role").and_then(Value::as_str),
        Some("assistant")
    );
    assert_eq!(
        items[0]["content"][0]["text"].as_str(),
        Some("Voy a buscar.")
    );
    assert_eq!(
        items[1].get("type").and_then(Value::as_str),
        Some("function_call")
    );
}

#[test]
fn test_responses_input_consecutive_system_messages() {
    let s1 = OpenAIMessage {
        role: "system".into(),
        content: Some(Value::String("Prompt 1".into())),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: Default::default(),
    };
    let s2 = OpenAIMessage {
        role: "system".into(),
        content: Some(Value::String("Prompt 2".into())),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: Default::default(),
    };
    let u = OpenAIMessage {
        role: "user".into(),
        content: Some(Value::String("hi".into())),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: Default::default(),
    };
    let all = [s1, s2, u];
    let (instructions, msgs) = extract_system_and_messages(&all);
    assert_eq!(instructions, Some("Prompt 1\n\nPrompt 2".into()));
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].role, "user");
}

#[test]
fn normalize_effort_returns_expected() {
    for (input, expected) in [
        ("max", "xhigh"),
        ("xhigh", "xhigh"),
        ("high", "high"),
        ("medium", "medium"),
        ("low", "low"),
        ("none", "none"),
        ("unknown", "medium"),
        ("", "medium"),
    ] {
        assert_eq!(super::normalize_effort(input), expected);
    }
}

#[test]
fn test_openai_formatter_strips_disabled_and_responses_keys() {
    let adapter = ProviderAdapterEnum::NvidiaNim(Box::new(NvidiaNimAdapter::new()));
    let mut extra = serde_json::Map::new();
    extra.insert("disabled".into(), json!(true));
    extra.insert("prompt_cache_key".into(), json!("pck_12345"));
    extra.insert("prompt_cache_retention".into(), json!("24h"));
    extra.insert("instructions".into(), json!("system instructions"));
    extra.insert("custom_val".into(), json!("ok"));

    let openai_req = OpenAIRequest {
        model: "test-model".into(),
        messages: vec![OpenAIMessage {
            role: "user".into(),
            content: Some(json!("hello")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        }],
        extra,
        ..Default::default()
    };
    let req = test_req(openai_req.clone());
    let formatted = OpenaiFormatter
        .format_request(&req, &test_model(), &openai_req.messages, true, &adapter)
        .expect("ok");
    let val: Value = serde_json::from_slice(&formatted).unwrap();
    assert!(val.get("disabled").is_none());
    assert!(val.get("prompt_cache_key").is_none());
    assert!(val.get("prompt_cache_retention").is_none());
    assert!(val.get("instructions").is_none());
    assert_eq!(val.get("custom_val"), Some(&json!("ok")));
}

#[test]
fn test_openai_formatter_sanitizes_message_names() {
    let adapter = ProviderAdapterEnum::NvidiaNim(Box::new(NvidiaNimAdapter::new()));
    let messages = vec![
        OpenAIMessage {
            role: "developer".into(),
            content: Some(json!("instruction")),
            name: Some("Dev Lead".into()),
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        },
        OpenAIMessage {
            role: "tool".into(),
            content: Some(json!("tool output")),
            name: Some("calc.run".into()),
            tool_call_id: Some("call_abc".into()),
            tool_calls: None,
            extra: Default::default(),
        },
        OpenAIMessage {
            role: "user".into(),
            content: Some(json!("hello")),
            name: Some(String::new()),
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        },
    ];
    let req = test_req(OpenAIRequest {
        model: "test-model".into(),
        messages: messages.clone(),
        ..Default::default()
    });
    let formatted = OpenaiFormatter
        .format_request(&req, &test_model(), &messages, false, &adapter)
        .expect("ok");
    let val: Value = serde_json::from_slice(&formatted).unwrap();
    let msgs = val.get("messages").unwrap().as_array().unwrap();
    assert_eq!(msgs[0].get("role").unwrap(), "system");
    assert_eq!(msgs[0].get("name").unwrap(), "Dev_Lead");
    assert_eq!(msgs[1].get("role").unwrap(), "tool");
    assert!(msgs[1].get("name").is_none());
    assert_eq!(msgs[2].get("role").unwrap(), "user");
    assert!(msgs[2].get("name").is_none());
}

#[test]
fn test_openai_formatter_flattens_assistant_array_content() {
    let adapter = ProviderAdapterEnum::NvidiaNim(Box::new(NvidiaNimAdapter::new()));
    let messages = vec![
        OpenAIMessage {
            role: "assistant".into(),
            content: Some(json!([
                {
                    "type": "output_text",
                    "text": "Voy a empezar cargando las skills necesarias.",
                    "annotations": []
                }
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        },
        OpenAIMessage {
            role: "assistant".into(),
            content: Some(json!([
                {
                    "type": "thinking",
                    "thinking": "Paso 1: Analizar.\n"
                },
                {
                    "type": "text",
                    "text": "Respuesta final."
                }
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        },
    ];
    let req = test_req(OpenAIRequest {
        model: "test-model".into(),
        messages: messages.clone(),
        ..Default::default()
    });
    let formatted = OpenaiFormatter
        .format_request(&req, &test_model(), &messages, false, &adapter)
        .expect("ok");
    let val: Value = serde_json::from_slice(&formatted).unwrap();
    let msgs = val.get("messages").unwrap().as_array().unwrap();

    assert_eq!(msgs[0].get("role").unwrap(), "assistant");
    assert_eq!(
        msgs[0].get("content").unwrap(),
        "Voy a empezar cargando las skills necesarias."
    );

    assert_eq!(msgs[1].get("role").unwrap(), "assistant");
    assert_eq!(
        msgs[1].get("content").unwrap(),
        "Paso 1: Analizar.\nRespuesta final."
    );
}

#[test]
fn test_openai_formatter_sanitizes_user_output_text_and_media() {
    let adapter = ProviderAdapterEnum::NvidiaNim(Box::new(NvidiaNimAdapter::new()));
    let messages = vec![
        OpenAIMessage {
            role: "user".into(),
            content: Some(json!([
                {
                    "type": "output_text",
                    "text": "Plain text prompt.",
                    "annotations": []
                }
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        },
        OpenAIMessage {
            role: "user".into(),
            content: Some(json!([
                {
                    "type": "output_text",
                    "text": "Image description.",
                    "annotations": []
                },
                {
                    "type": "image_url",
                    "image_url": { "url": "https://example.com/img.png" }
                }
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        },
    ];
    let req = test_req(OpenAIRequest {
        model: "test-model".into(),
        messages: messages.clone(),
        ..Default::default()
    });
    let formatted = OpenaiFormatter
        .format_request(&req, &test_model(), &messages, false, &adapter)
        .expect("ok");
    let val: Value = serde_json::from_slice(&formatted).unwrap();
    let msgs = val.get("messages").unwrap().as_array().unwrap();

    // Pure text user message gets flattened to plain string
    assert_eq!(msgs[0].get("content").unwrap(), "Plain text prompt.");

    // Multimodal user message keeps array, converts output_text -> text, strips annotations
    let parts = msgs[1].get("content").unwrap().as_array().unwrap();
    assert_eq!(parts[0].get("type").unwrap(), "text");
    assert_eq!(parts[0].get("text").unwrap(), "Image description.");
    assert!(parts[0].get("annotations").is_none());
    assert_eq!(parts[1].get("type").unwrap(), "image_url");
}

#[test]
fn test_openai_formatter_strips_cache_control_from_messages_tools_and_extra() {
    let adapter = ProviderAdapterEnum::NvidiaNim(Box::new(NvidiaNimAdapter::new()));
    let mut msg_extra = serde_json::Map::new();
    msg_extra.insert("cache_control".into(), json!({"type": "ephemeral"}));

    let mut req_extra = serde_json::Map::new();
    req_extra.insert("cache_control".into(), json!({"type": "ephemeral"}));
    req_extra.insert("custom_allowed".into(), json!("ok"));

    let messages = vec![
        OpenAIMessage {
            role: "user".into(),
            content: Some(Value::String("hello".into())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: msg_extra,
        },
        OpenAIMessage {
            role: "assistant".into(),
            content: Some(Value::String("world".into())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        },
    ];

    let tool_with_cache = json!({
        "type": "function",
        "cache_control": {"type": "ephemeral"},
        "function": {
            "name": "search",
            "description": "web search",
            "parameters": {}
        }
    });

    let req = test_req(OpenAIRequest {
        model: "test-model".into(),
        messages: messages.clone(),
        tools: Some(vec![tool_with_cache]),
        extra: req_extra,
        ..Default::default()
    });

    let formatted = OpenaiFormatter
        .format_request(&req, &test_model(), &messages, false, &adapter)
        .expect("ok");
    let val: Value = serde_json::from_slice(&formatted).unwrap();

    // 1. Message extra does not contain cache_control
    let msgs = val.get("messages").unwrap().as_array().unwrap();
    assert!(msgs[0].get("cache_control").is_none(), "cache_control must be stripped from message");

    // 2. Tool does not contain cache_control
    let tools = val.get("tools").unwrap().as_array().unwrap();
    assert!(tools[0].get("cache_control").is_none(), "cache_control must be stripped from tool");

    // 3. Top-level extra does not contain cache_control, but keeps custom_allowed
    assert!(val.get("cache_control").is_none(), "cache_control must be stripped from top-level extra");
    assert_eq!(val.get("custom_allowed").unwrap(), "ok");
}
