use crate::translation::*;
use openproxy_adapters::adapters::gemini::*;
use openproxy_types::{OpenAIMessage, OpenAIRequest};
use serde_json::{Value, json};

fn openai_req_with(messages: Vec<(&str, &str)>) -> OpenAIRequest {
    OpenAIRequest {
        model: "claude-test".into(),
        messages: messages
            .into_iter()
            .map(|(r, c)| OpenAIMessage {
                role: r.into(),
                content: Some(Value::String(c.into())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            })
            .collect(),
        stream: false,
        temperature: Some(0.5),
        max_tokens: None,
        top_p: None,
        stop: None,
        tools: None,
        tool_choice: None,
        top_k: None,
        user: None,
        extra: serde_json::Map::new(),
    }
}

fn make_msg(
    role: &str,
    content: Option<Value>,
    tool_call_id: Option<&str>,
    tool_calls: Option<Vec<Value>>,
) -> OpenAIMessage {
    OpenAIMessage {
        role: role.into(),
        content,
        name: None,
        tool_call_id: tool_call_id.map(String::from),
        tool_calls,
        extra: serde_json::Map::new(),
    }
}

#[test]
fn minimax_tools_with_empty_name_are_filtered_out() {
    let mut req = openai_req_with(vec![("user", "go")]);
    req.tools = Some(vec![
        json!({"type": "function", "function": {"name": "valid_tool", "description": "fine", "parameters": {"type": "object"}}}),
        json!({"type": "function", "function": {"name": "", "description": "empty", "parameters": {"type": "object"}}}),
        json!({"type": "function", "function": {"description": "no name", "parameters": {"type": "object"}}}),
    ]);
    let tools = openai_to_anthropic(&req, "c", &req.messages, false)
        .tools
        .unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "valid_tool");
}

#[test]
fn minimax_assistant_tool_calls_translated_to_tool_use_blocks() {
    let mut req = openai_req_with(vec![]);
    req.messages = vec![
        make_msg(
            "user",
            Some(json!("What's the weather in Paris?")),
            None,
            None,
        ),
        make_msg(
            "assistant",
            Some(Value::Null),
            None,
            Some(vec![
                json!({"id": "call_abc123", "type": "function", "function": { "name": "get_weather", "arguments": "{\"city\": \"Paris\"}" }}),
            ]),
        ),
    ];
    let out = openai_to_anthropic(&req, "c", &req.messages, false);
    let blocks = out.messages[1].content.as_array().unwrap();
    assert_eq!(
        (
            blocks[0]["type"].as_str(),
            blocks[0]["id"].as_str(),
            blocks[0]["name"].as_str()
        ),
        (Some("tool_use"), Some("call_abc123"), Some("get_weather"))
    );
    assert_eq!(blocks[0]["input"]["city"], "Paris");
}

#[test]
fn minimax_tool_role_messages_translated_to_tool_result_blocks() {
    let mut req = openai_req_with(vec![]);
    req.messages = vec![
        make_msg("user", Some(json!("weather?")), None, None),
        make_msg(
            "assistant",
            Some(Value::Null),
            None,
            Some(vec![
                json!({"id": "call_xyz", "type": "function", "function": {"name": "get_weather", "arguments": "{}"}}),
            ]),
        ),
        make_msg(
            "tool",
            Some(json!("{\"temp\": 18}")),
            Some("call_xyz"),
            None,
        ),
    ];
    let out = openai_to_anthropic(&req, "c", &req.messages, false);
    let blocks = out.messages[2].content.as_array().unwrap();
    assert_eq!(
        (
            blocks[0]["type"].as_str(),
            blocks[0]["tool_use_id"].as_str()
        ),
        (Some("tool_result"), Some("call_xyz"))
    );
    assert_eq!(blocks[0]["content"], "{\"temp\": 18}");
}

#[test]
fn minimax_assistant_tool_calls_with_empty_name_are_skipped() {
    let mut req = openai_req_with(vec![]);
    req.messages = vec![
        make_msg("user", Some(json!("go")), None, None),
        make_msg(
            "assistant",
            Some(json!("Thinking...")),
            None,
            Some(vec![
                json!({"id": "call_1", "type": "function", "function": {"name": "", "arguments": "{}"}}),
                json!({"id": "call_2", "type": "function", "function": {"name": "valid_tool", "arguments": "{\"x\":1}"}}),
            ]),
        ),
    ];
    let blocks = openai_to_anthropic(&req, "c", &req.messages, false).messages[1]
        .content
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(
        (blocks[0]["type"].as_str(), blocks[0]["text"].as_str()),
        (Some("text"), Some("Thinking..."))
    );
    assert_eq!(
        (blocks[1]["type"].as_str(), blocks[1]["name"].as_str()),
        (Some("tool_use"), Some("valid_tool"))
    );
}

#[test]
fn minimax_tool_calls_with_empty_arguments_become_empty_object() {
    let mut req = openai_req_with(vec![]);
    req.messages = vec![make_msg(
        "assistant",
        Some(Value::Null),
        None,
        Some(vec![
            json!({"id": "call_1", "type": "function", "function": {"name": "no_args_tool", "arguments": ""}}),
        ]),
    )];
    let blocks = openai_to_anthropic(&req, "c", &req.messages, false).messages[0]
        .content
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(
        (blocks[0]["type"].as_str(), &blocks[0]["input"]),
        (Some("tool_use"), &json!({}))
    );
}

#[test]
fn minimax_full_tool_round_trip_request_shape() {
    let mut req = openai_req_with(vec![]);
    req.model = "MiniMax-M3".into();
    req.tools = Some(vec![
        json!({"type": "function", "function": {"name": "get_weather", "description": "Get weather for a city", "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}}}),
    ]);
    req.tool_choice = Some(json!("auto"));
    req.messages = vec![
        make_msg(
            "user",
            Some(json!("What's the weather in Paris?")),
            None,
            None,
        ),
        make_msg(
            "assistant",
            Some(Value::Null),
            None,
            Some(vec![
                json!({"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}),
            ]),
        ),
        make_msg(
            "tool",
            Some(json!("{\"temp\":18,\"unit\":\"c\"}")),
            Some("call_1"),
            None,
        ),
    ];
    let out = openai_to_anthropic(&req, "c", &req.messages, false);
    assert_eq!(out.tools.as_ref().unwrap()[0]["name"], "get_weather");
    assert_eq!(out.tool_choice.as_ref().unwrap(), &json!({"type": "auto"}));
    assert_eq!(out.messages.len(), 3);
    assert_eq!(
        out.messages[1].content.as_array().unwrap()[0]["name"],
        "get_weather"
    );
    assert_eq!(
        out.messages[2].content.as_array().unwrap()[0]["content"],
        "{\"temp\":18,\"unit\":\"c\"}"
    );
}

#[test]
fn h4_top_k_passes_through_to_anthropic() {
    let mut req = openai_req_with(vec![("user", "go")]);
    req.top_k = Some(40);
    assert_eq!(
        openai_to_anthropic(&req, "c", &req.messages, false).top_k,
        Some(40)
    );
}

#[test]
fn minimax_tool_result_then_user_merges_into_single_user_message() {
    let mut req = openai_req_with(vec![]);
    req.messages = vec![
        make_msg("user", Some(json!("que falta?")), None, None),
        make_msg(
            "assistant",
            Some(json!("Veo cómo se renderiza:")),
            None,
            None,
        ),
        make_msg(
            "assistant",
            Some(json!("Eso no es el render del bloque...")),
            None,
            None,
        ),
        make_msg(
            "assistant",
            Some(json!("El bloque core/html no tiene render_callback...")),
            None,
            None,
        ),
        make_msg(
            "user",
            Some(json!(
                "You've reached the maximum number of tool-calling iterations."
            )),
            None,
            None,
        ),
        make_msg(
            "assistant",
            Some(json!("**0 placeholders sin reemplazar.**")),
            None,
            None,
        ),
        make_msg(
            "assistant",
            Some(Value::Null),
            None,
            Some(vec![
                json!({"id":"call_A","type":"function","function":{"name":"tool_a","arguments":"{}"}}),
            ]),
        ),
        make_msg("tool", Some(json!("result A")), Some("call_A"), None),
        make_msg(
            "assistant",
            Some(Value::Null),
            None,
            Some(vec![
                json!({"id":"call_B","type":"function","function":{"name":"tool_b","arguments":"{}"}}),
            ]),
        ),
        make_msg("tool", Some(json!("result B")), Some("call_B"), None),
        make_msg(
            "assistant",
            Some(Value::Null),
            None,
            Some(vec![
                json!({"id":"call_C","type":"function","function":{"name":"tool_c","arguments":"{}"}}),
            ]),
        ),
        make_msg("tool", Some(json!("result C")), Some("call_C"), None),
        make_msg("user", Some(json!("no se que mecanismo es...")), None, None),
    ];
    let out = openai_to_anthropic(&req, "c", &req.messages, false);
    for (i, w) in out.messages.windows(2).enumerate() {
        assert_ne!(w[1].role, w[0].role, "consecutive same role at {i}");
    }
    let last = &out.messages[8];
    assert_eq!(last.role, "user");
    let blocks = last.content.as_array().unwrap();
    assert_eq!(
        (
            blocks[0]["type"].as_str(),
            blocks[0]["tool_use_id"].as_str(),
            blocks[0]["content"].as_str()
        ),
        (Some("tool_result"), Some("call_C"), Some("result C"))
    );
    assert_eq!(
        (blocks[1]["type"].as_str(), blocks[1]["text"].as_str()),
        (Some("text"), Some("no se que mecanismo es..."))
    );
}

#[test]
fn h4_user_field_maps_to_anthropic_metadata_user_id() {
    let mut req = openai_req_with(vec![("user", "go")]);
    req.user = Some("user-abc-123".into());
    assert_eq!(
        openai_to_anthropic(&req, "c", &req.messages, false)
            .metadata
            .unwrap()["user_id"],
        "user-abc-123"
    );
}

#[test]
fn h4_absent_optional_fields_default_to_none() {
    let req = openai_req_with(vec![("user", "hi")]);
    let out = openai_to_anthropic(&req, "c", &req.messages, false);
    assert!(
        out.tools.is_none()
            && out.tool_choice.is_none()
            && out.top_k.is_none()
            && out.metadata.is_none()
    );
}

#[test]
fn parse_image_url_to_inline_data_extracts_base64() {
    let part = json!({"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,f00bar"}});
    let r = parse_image_url_to_inline_data(&part).unwrap();
    assert_eq!(
        (r.mime_type.as_str(), r.data.as_str()),
        ("image/jpeg", "f00bar")
    );
    assert!(parse_image_url_to_inline_data(&json!({"type": "text", "text": "hello"})).is_none());
    assert!(
        parse_image_url_to_inline_data(
            &json!({"type": "image_url", "image_url": {"url": "https://example.com/image.jpg"}})
        )
        .is_none()
    );
}

#[tokio::test]
async fn openai_to_anthropic_sse_stream_translates_chunks() {
    use futures_util::StreamExt;
    let chunks = vec![
        bytes::Bytes::from("data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n"),
        bytes::Bytes::from("data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n"),
        bytes::Bytes::from("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"),
        bytes::Bytes::from("data: [DONE]\n\n"),
    ];
    let sse_stream = OpenAIToAnthropicSseStream::new(
        futures_util::stream::iter(chunks),
        "msg_test".into(),
        "claude-3".into(),
    );
    let results: Vec<bytes::Bytes> = sse_stream.map(|r| r.unwrap()).collect().await;
    assert_eq!(results.len(), 3);
    let s0 = std::str::from_utf8(&results[0]).unwrap();
    assert!(s0.contains("event: message_start") && s0.contains("Hello"));
    assert!(std::str::from_utf8(&results[1]).unwrap().contains("world"));
    assert!(
        std::str::from_utf8(&results[2])
            .unwrap()
            .contains("event: message_stop")
    );
}

#[test]
fn anthropic_to_openai_normalizes_cache_tokens() {
    let raw = json!({
        "id": "msg_123", "type": "message", "role": "assistant",
        "content": [{"type": "text", "text": "Cached response"}],
        "model": "claude-3-5-sonnet", "stop_reason": "end_turn",
        "usage": { "input_tokens": 50, "output_tokens": 20, "cache_read_input_tokens": 1000, "cache_creation_input_tokens": 200 }
    });
    let out = anthropic_to_openai(&serde_json::from_value(raw).unwrap());
    let u = out.usage.unwrap();
    assert_eq!(
        (u.prompt_tokens, u.completion_tokens, u.total_tokens),
        (1250, 20, 1270)
    );
    assert_eq!(
        u.prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens),
        Some(1000)
    );
}

#[test]
fn anthropic_sse_forwards_cached_usage() {
    let event = AnthropicSseEvent::MessageDelta {
        delta: json!({"stop_reason": "end_turn"}),
        usage: Some(crate::translation::types::AnthropicUsage {
            input_tokens: 50,
            output_tokens: 100,
            cache_read_input_tokens: Some(500),
            cache_creation_input_tokens: Some(100),
        }),
    };
    let chunks = anthropic_sse_to_openai_chunks(&event, "cmpl_1", 123456, "claude-3-5-sonnet");
    assert_eq!(chunks.len(), 1);
    let v: Value = serde_json::from_str(chunks[0].strip_prefix("data: ").unwrap().trim()).unwrap();
    assert_eq!(
        (
            v["usage"]["prompt_tokens"].as_u64(),
            v["usage"]["completion_tokens"].as_u64(),
            v["usage"]["total_tokens"].as_u64()
        ),
        (Some(650), Some(100), Some(750))
    );
    assert_eq!(v["usage"]["prompt_tokens_details"]["cached_tokens"], 500);
}
