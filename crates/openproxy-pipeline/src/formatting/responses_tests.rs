use super::test_helpers::*;
use super::*;
use openproxy_adapters::adapters::ProviderAdapterEnum;
use openproxy_adapters::adapters::nvidia_nim::NvidiaNimAdapter;
use openproxy_types::{OpenAIMessage, OpenAIRequest, TargetFormat};

#[test]
fn test_responses_formatter_preserves_prompt_cache_key_and_developer_instructions() {
    let adapter = ProviderAdapterEnum::NvidiaNim(Box::new(NvidiaNimAdapter::new()));
    let mut extra = serde_json::Map::new();
    extra.insert("prompt_cache_key".into(), json!("client-cache-key-999"));
    extra.insert("max_completion_tokens".into(), json!(1000));

    let messages = vec![
        OpenAIMessage {
            role: "system".into(),
            content: Some(json!("You are an expert assistant.")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        },
        OpenAIMessage {
            role: "user".into(),
            content: Some(json!("Hello")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        },
    ];

    let req = test_req(OpenAIRequest {
        model: "gpt-4o".into(),
        messages: messages.clone(),
        extra,
        ..Default::default()
    });

    let mut model = test_model();
    model.target_format = TargetFormat::Responses;

    let formatted = ResponsesFormatter
        .format_request(&req, &model, &messages, false, &adapter)
        .expect("formatted");
    let val: Value = serde_json::from_slice(&formatted).unwrap();

    assert_eq!(
        val.get("prompt_cache_key").and_then(Value::as_str),
        Some("client-cache-key-999")
    );
    assert!(
        val.get("max_completion_tokens").is_none(),
        "ResponsesFormatter must strip max_completion_tokens"
    );

    // Verify system instructions are preserved in input as role developer
    let input = val.get("input").unwrap().as_array().unwrap();
    assert_eq!(input.len(), 2);
    assert_eq!(
        input[0].get("role").and_then(Value::as_str),
        Some("developer")
    );
    assert_eq!(
        input[0]["content"][0]["text"].as_str(),
        Some("You are an expert assistant.")
    );
    assert_eq!(input[1].get("role").and_then(Value::as_str), Some("user"));
}

#[test]
fn test_responses_formatter_assistant_reasoning_reinjection() {
    let mut extra = serde_json::Map::new();
    extra.insert(
        "reasoning_content".into(),
        json!("Step-by-step thinking process."),
    );

    let messages = [
        OpenAIMessage {
            role: "user".into(),
            content: Some(json!("Solve problem")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        },
        OpenAIMessage {
            role: "assistant".into(),
            content: Some(json!("Final solution")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra,
        },
    ];

    let refs: Vec<&OpenAIMessage> = messages.iter().collect();
    let input_val = messages_to_responses_input(&refs);
    let items = input_val.as_array().expect("array");

    // items should be: user message, reasoning item, assistant message
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].get("role").and_then(Value::as_str), Some("user"));

    assert_eq!(
        items[1].get("type").and_then(Value::as_str),
        Some("reasoning")
    );
    assert_eq!(
        items[1]
            .get("summary")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        items[1]["summary"][0]["type"].as_str(),
        Some("summary_text")
    );
    assert_eq!(
        items[1]["summary"][0]["text"].as_str(),
        Some("Step-by-step thinking process.")
    );
    assert!(items[1].get("content").is_none());

    assert_eq!(
        items[2].get("role").and_then(Value::as_str),
        Some("assistant")
    );
    assert_eq!(
        items[2]["content"][0]["text"].as_str(),
        Some("Final solution")
    );
}

#[test]
fn test_format_responses_tools_sanitizes_subagent_schema() {
    let raw_tools = json!([{
        "type": "function",
        "function": {
            "name": "subagent",
            "description": "Spawns a subagent",
            "parameters": {
                "type": "object",
                "properties": {
                    "agent": { "type": "string" },
                    "prompt": { "type": "string" },
                    "description": { "type": "string" },
                    "sessionID": {
                        "type": "string",
                        "pattern": "^ses"
                    }
                },
                "required": ["agent", "prompt", "description"],
                "additionalProperties": false
            }
        }
    }]);

    let tools_slice = raw_tools.as_array().map(Vec::as_slice);
    let formatted = format_responses_tools(tools_slice).expect("formatted tools");
    let tool_obj = &formatted[0];

    assert_eq!(tool_obj["type"], "function");
    assert_eq!(tool_obj["name"], "subagent");

    let params = &tool_obj["parameters"];
    // Regex pattern should be stripped
    assert!(
        !params["properties"]["sessionID"]
            .as_object()
            .unwrap()
            .contains_key("pattern")
    );
    // additionalProperties: false should be relaxed/removed since sessionID is optional
    assert!(
        !params
            .as_object()
            .unwrap()
            .contains_key("additionalProperties")
    );
}

#[test]
fn test_responses_formatter_developer_instruction_preservation_and_cache_key() {
    let adapter = ProviderAdapterEnum::NvidiaNim(Box::new(NvidiaNimAdapter::new()));
    let messages = [
        OpenAIMessage {
            role: "developer".into(),
            content: Some(json!("You are an elite coding subagent.")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        },
        OpenAIMessage {
            role: "user".into(),
            content: Some(json!("Write tests.")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        },
    ];

    let req = test_req(OpenAIRequest {
        model: "codex-test".into(),
        messages: messages.to_vec(),
        extra: Default::default(),
        ..Default::default()
    });

    let mut model = test_model();
    model.target_format = TargetFormat::Responses;

    let formatted = ResponsesFormatter
        .format_request(&req, &model, &messages, false, &adapter)
        .expect("formatted");
    let val: Value = serde_json::from_slice(&formatted).unwrap();

    assert_eq!(
        val.get("instructions").and_then(Value::as_str),
        Some("You are an elite coding subagent.")
    );

    // Verify developer message is preserved in input[0]
    let input = val.get("input").unwrap().as_array().unwrap();
    assert_eq!(
        input[0].get("role").and_then(Value::as_str),
        Some("developer")
    );
    assert_eq!(
        input[0]["content"][0]["text"].as_str(),
        Some("You are an elite coding subagent.")
    );

    // Verify prompt_cache_key is generated
    let pck = val.get("prompt_cache_key").and_then(Value::as_str).unwrap();
    assert!(pck.starts_with("pck_"));
}

#[test]
fn test_responses_formatter_sanitizes_reasoning_content_array() {
    let adapter = ProviderAdapterEnum::NvidiaNim(Box::new(NvidiaNimAdapter::new()));
    let messages = [
        OpenAIMessage {
            role: "user".into(),
            content: Some(json!("hello")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        },
        OpenAIMessage {
            role: "assistant".into(),
            content: Some(json!("hi")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::from_str(r#"{"reasoning_content":"inner thought"}"#).unwrap(),
        },
    ];

    let req = test_req(OpenAIRequest {
        model: "gpt-5.6-sol".into(),
        messages: messages.to_vec(),
        extra: Default::default(),
        ..Default::default()
    });

    let mut model = test_model();
    model.target_format = TargetFormat::Responses;

    let formatted = ResponsesFormatter
        .format_request(&req, &model, &messages, false, &adapter)
        .expect("formatted");
    let val: Value = serde_json::from_slice(&formatted).unwrap();
    let input = val.get("input").unwrap().as_array().unwrap();

    let reasoning_item = input
        .iter()
        .find(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"))
        .expect("reasoning item present");

    assert!(reasoning_item.get("content").is_none());
    assert_eq!(
        reasoning_item["summary"][0]["text"].as_str(),
        Some("inner thought")
    );
}

#[test]
fn test_responses_formatter_formats_image_inputs_correctly() {
    let adapter = ProviderAdapterEnum::NvidiaNim(Box::new(NvidiaNimAdapter::new()));
    let messages = [OpenAIMessage {
        role: "user".into(),
        content: Some(json!([
            { "type": "text", "text": "What is in this picture?" },
            { "type": "image_url", "image_url": { "url": "data:image/jpeg;base64,dGVzdA==" } },
            { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "YW50aHJvcGlj" } }
        ])),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: Default::default(),
    }];

    let req = test_req(OpenAIRequest {
        model: "gpt-6.1-sol".into(),
        messages: messages.to_vec(),
        extra: Default::default(),
        ..Default::default()
    });

    let mut model = test_model();
    model.target_format = TargetFormat::Responses;

    let formatted = ResponsesFormatter
        .format_request(&req, &model, &messages, false, &adapter)
        .expect("formatted");
    let val: Value = serde_json::from_slice(&formatted).unwrap();
    let input = val.get("input").unwrap().as_array().unwrap();

    let user_msg = input
        .iter()
        .find(|item| item.get("role").and_then(Value::as_str) == Some("user"))
        .expect("user message present");
    let content = user_msg["content"].as_array().expect("content array");

    assert_eq!(content[0]["type"], "input_text");
    assert_eq!(content[0]["text"], "What is in this picture?");

    assert_eq!(content[1]["type"], "input_image");
    assert_eq!(content[1]["image_url"], "data:image/jpeg;base64,dGVzdA==");
    assert!(content[1].get("image").is_none(), "Must not emit 'image'");
    assert!(
        content[1].get("mime_type").is_none(),
        "Must not emit 'mime_type'"
    );

    assert_eq!(content[2]["type"], "input_image");
    assert_eq!(
        content[2]["image_url"],
        "data:image/png;base64,YW50aHJvcGlj"
    );
    assert!(content[2].get("image").is_none(), "Must not emit 'image'");
    assert!(
        content[2].get("mime_type").is_none(),
        "Must not emit 'mime_type'"
    );
}

#[test]
fn anthropic_formatter_raises_max_tokens_to_model_ceiling_when_client_omits_it() {
    let adapter = ProviderAdapterEnum::NvidiaNim(Box::new(NvidiaNimAdapter::new()));
    let user = OpenAIMessage {
        role: "user".into(),
        content: Some(json!("hi")),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: Default::default(),
    };

    // Model declares a real output ceiling well above DEFAULT_MAX_TOKENS (4096).
    let mut model = test_model();
    model.target_format = TargetFormat::Anthropic;
    model.max_output_tokens = Some(131_072);

    // Client omits max_tokens => formatter raises emitted 4096 up to the ceiling.
    let req = test_req(OpenAIRequest {
        model: "test-model".into(),
        messages: vec![user.clone()],
        ..Default::default()
    });
    let body = AnthropicFormatter
        .format_request(&req, &model, &req.openai_request.messages, false, &adapter)
        .expect("ok");
    let val: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(val["max_tokens"], json!(131_072));

    // Client pins max_tokens => formatter must not override the client's choice.
    let req_pinned = test_req(OpenAIRequest {
        model: "test-model".into(),
        messages: vec![user.clone()],
        max_tokens: Some(1000),
        ..Default::default()
    });
    let body_pinned = AnthropicFormatter
        .format_request(
            &req_pinned,
            &model,
            &req_pinned.openai_request.messages,
            false,
            &adapter,
        )
        .expect("ok");
    let val_pinned: Value = serde_json::from_slice(&body_pinned).unwrap();
    assert_eq!(val_pinned["max_tokens"], json!(1000));

    // No declared ceiling => DEFAULT_MAX_TOKENS (4096) stays untouched.
    let mut no_ceiling = test_model();
    no_ceiling.target_format = TargetFormat::Anthropic;
    no_ceiling.max_output_tokens = None;
    let req_nc = test_req(OpenAIRequest {
        model: "test-model".into(),
        messages: vec![user],
        ..Default::default()
    });
    let body_nc = AnthropicFormatter
        .format_request(
            &req_nc,
            &no_ceiling,
            &req_nc.openai_request.messages,
            false,
            &adapter,
        )
        .expect("ok");
    let val_nc: Value = serde_json::from_slice(&body_nc).unwrap();
    assert_eq!(
        val_nc["max_tokens"],
        json!(crate::translation::types::DEFAULT_MAX_TOKENS)
    );
}
