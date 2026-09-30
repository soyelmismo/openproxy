use super::*;
use serde_json::json;

mod adversarial_tests;
mod adversarial_tools;
mod tools;

#[test]
fn test_parse_image_url_to_inline_data() {
    let part = json!({
        "type": "image_url",
        "image_url": { "url": "data:image/png;base64,iVBORw0KGgo=" }
    });
    let result = parse_image_url_to_inline_data(&part).unwrap();
    assert_eq!(result.mime_type, "image/png");
    assert_eq!(result.data, "iVBORw0KGgo=");
}

#[test]
fn test_parse_audio_parts_to_inline_data() {
    let input_audio = json!({
        "type": "input_audio",
        "input_audio": {
            "data": "UklGRigAAABXQVZFZm10IBAAAAABAAEAQB8AAEAfAAABAAgAZGF0YQAAAAA=",
            "format": "wav"
        }
    });
    let result = parse_media_part_to_inline_data(&input_audio).unwrap();
    assert_eq!(result.mime_type, "audio/wav");
    assert_eq!(
        result.data,
        "UklGRigAAABXQVZFZm10IBAAAAABAAEAQB8AAEAfAAABAAgAZGF0YQAAAAA="
    );

    let audio_url = json!({
        "type": "audio_url",
        "audio_url": {
            "url": "data:audio/mp3;base64,//uQZAAAAAAAAAAAAAAAAAAAAAA=="
        }
    });
    let result = parse_media_part_to_inline_data(&audio_url).unwrap();
    assert_eq!(result.mime_type, "audio/mp3");
    assert_eq!(result.data, "//uQZAAAAAAAAAAAAAAAAAAAAAA==");
}

#[test]
fn test_gemini_to_openai_standard() {
    let resp = GeminiResponse {
        candidates: vec![GeminiCandidate {
            content: Some(GeminiContent {
                role: "model".to_string(),
                parts: vec![GeminiPart {
                    text: Some("Hello from Gemini".to_string()),
                    inline_data: None,
                    ..Default::default()
                }],
            }),
            finish_reason: Some("STOP".to_string()),
        }],
        usage_metadata: Some(GeminiUsageMetadata {
            prompt_token_count: 10,
            candidates_token_count: 20,
            total_token_count: 30,
            cached_content_token_count: None,
        }),
        response: None,
    };

    let openai_resp = gemini_to_openai(&resp);
    assert_eq!(openai_resp.object, "chat.completion");
    assert_eq!(openai_resp.choices.len(), 1);

    let choice = &openai_resp.choices[0];
    assert_eq!(choice.finish_reason.as_deref(), Some("stop"));
    assert_eq!(choice.message.role, "assistant");
    assert_eq!(
        choice.message.content.as_ref().unwrap().as_str().unwrap(),
        "Hello from Gemini"
    );

    let usage = openai_resp.usage.unwrap();
    assert_eq!(usage.prompt_tokens, 10);
    assert_eq!(usage.completion_tokens, 20);
    assert_eq!(usage.total_tokens, 30);
}

#[test]
fn test_openai_to_gemini_string_and_array_content() {
    let req = openproxy_types::OpenAIRequest {
        model: "gemini-2.5-pro".to_string(),
        messages: vec![],
        ..Default::default()
    };
    let messages = vec![
        openproxy_types::OpenAIMessage {
            role: "system".to_string(),
            content: Some(json!("You are helpful")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::default(),
        },
        openproxy_types::OpenAIMessage {
            role: "user".to_string(),
            content: Some(json!("Direct string")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::default(),
        },
        openproxy_types::OpenAIMessage {
            role: "assistant".to_string(),
            content: Some(json!([
                {"type": "text", "text": "Part 1 "},
                {"type": "text", "content": "Part 2"}
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::default(),
        },
    ];

    let gemini_req = openai_to_gemini(&req, &messages);
    assert_eq!(
        gemini_req.system_instruction.unwrap().parts[0]
            .text
            .as_deref(),
        Some("You are helpful")
    );
    assert_eq!(gemini_req.contents.len(), 2);
    assert_eq!(gemini_req.contents[0].role, "user");
    assert_eq!(
        gemini_req.contents[0].parts[0].text.as_deref(),
        Some("Direct string")
    );
    assert_eq!(gemini_req.contents[1].role, "model");
    assert_eq!(
        gemini_req.contents[1].parts[0].text.as_deref(),
        Some("Part 1 ")
    );
    assert_eq!(
        gemini_req.contents[1].parts[1].text.as_deref(),
        Some("Part 2")
    );
}

#[test]
fn test_gemini_finish_reason_mapping() {
    assert_eq!(map_gemini_finish_reason("MAX_TOKENS"), "length");
    assert_eq!(map_gemini_finish_reason("SAFETY"), "content_filter");
    assert_eq!(map_gemini_finish_reason("RECITATION"), "content_filter");
    assert_eq!(map_gemini_finish_reason("BLOCKLIST"), "content_filter");
    assert_eq!(map_gemini_finish_reason("STOP"), "stop");
    assert_eq!(map_gemini_finish_reason("OTHER"), "stop");
}

#[test]
fn test_gemini_to_openai_multipart() {
    let resp = GeminiResponse {
        candidates: vec![GeminiCandidate {
            content: Some(GeminiContent {
                role: "model".to_string(),
                parts: vec![
                    GeminiPart {
                        text: Some("Hello ".to_string()),
                        inline_data: None,
                        ..Default::default()
                    },
                    GeminiPart {
                        text: Some("world!".to_string()),
                        inline_data: None,
                        ..Default::default()
                    },
                ],
            }),
            finish_reason: Some("STOP".to_string()),
        }],
        usage_metadata: None,
        response: None,
    };

    let openai_resp = gemini_to_openai(&resp);
    assert_eq!(
        openai_resp.choices[0]
            .message
            .content
            .as_ref()
            .unwrap()
            .as_str()
            .unwrap(),
        "Hello world!"
    );
}

#[test]
fn test_openai_to_gemini_multiple_system_messages() {
    let req = openproxy_types::OpenAIRequest {
        model: "gemini-2.5-pro".to_string(),
        messages: vec![],
        ..Default::default()
    };
    let messages = vec![
        openproxy_types::OpenAIMessage {
            role: "system".to_string(),
            content: Some(json!("System prompt 1")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::default(),
        },
        openproxy_types::OpenAIMessage {
            role: "system".to_string(),
            content: Some(json!("System prompt 2")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::default(),
        },
    ];

    let gemini_req = openai_to_gemini(&req, &messages);
    assert_eq!(
        gemini_req.system_instruction.unwrap().parts[0]
            .text
            .as_deref(),
        Some("System prompt 1\n\nSystem prompt 2")
    );
}

#[test]
fn test_gemini_model_supports_thinking_filter() {
    // Non-thinking models: Gemma, Gemini 1.x, Lite, embeddings
    assert!(!gemini_model_supports_thinking("gemma-4-26b-a4b-it"));
    assert!(!gemini_model_supports_thinking("models/gemma-4-26b-a4b-it"));
    assert!(!gemini_model_supports_thinking("google/gemma-2-27b-it"));
    assert!(!gemini_model_supports_thinking("gemini-1.5-pro"));
    assert!(!gemini_model_supports_thinking("gemini-1.5-flash"));
    assert!(!gemini_model_supports_thinking("gemini-1.5-flash-8b"));
    assert!(!gemini_model_supports_thinking("gemini-pro"));
    assert!(!gemini_model_supports_thinking("gemini-2.0-flash-lite"));
    assert!(!gemini_model_supports_thinking("text-embedding-004"));
    assert!(!gemini_model_supports_thinking(""));

    // Thinking models: Gemini 2.0+, Gemini 2.5+, Gemini 3+, explicit thinking models
    assert!(gemini_model_supports_thinking("gemini-2.5-flash"));
    assert!(gemini_model_supports_thinking("gemini-2.5-pro"));
    assert!(gemini_model_supports_thinking("gemini-2.0-flash"));
    assert!(gemini_model_supports_thinking(
        "gemini-2.0-flash-thinking-exp-01-21"
    ));
    assert!(gemini_model_supports_thinking("gemini-3.0-pro"));
    assert!(gemini_model_supports_thinking("gemini-exp-1206"));
}

#[test]
fn test_openai_to_gemini_with_model_omits_thinking_for_gemma() {
    let mut extra = serde_json::Map::new();
    extra.insert("reasoning_effort".to_string(), json!("high"));
    let req = openproxy_types::OpenAIRequest {
        model: "gemma-4-26b-a4b-it".into(),
        extra,
        ..Default::default()
    };

    // Gemma model: thinkingConfig MUST NOT be present
    let gemma_req = openai_to_gemini_with_model(&req, &[], Some("gemma-4-26b-a4b-it"));
    assert!(
        gemma_req
            .generation_config
            .as_ref()
            .unwrap()
            .thinking_config
            .is_none(),
        "Gemma must omit thinkingConfig to avoid HTTP 400"
    );

    // Gemini 2.5 model: thinkingConfig SHOULD be present
    let gemini_req = openai_to_gemini_with_model(&req, &[], Some("gemini-2.5-pro"));
    assert_eq!(
        gemini_req
            .generation_config
            .as_ref()
            .unwrap()
            .thinking_config
            .as_ref()
            .unwrap()
            .thinking_budget,
        16384
    );
}

#[test]
fn test_gemini_adapter_wrap_request_body_strips_thinking_config_for_gemma() {
    let adapter = GeminiAdapter::new();
    let body_json = json!({
        "contents": [{"parts": [{"text": "Hello"}]}],
        "generationConfig": {
            "temperature": 0.7,
            "thinkingConfig": {
                "thinkingBudget": 8192
            }
        }
    });
    let body_bytes = bytes::Bytes::from(serde_json::to_vec(&body_json).unwrap());
    let resolved_target = openproxy_types::context::ResolvedTarget {
        target: openproxy_types::combos::ComboTarget {
            id: openproxy_types::ComboTargetId(1),
            combo_id: openproxy_types::ComboId(1),
            provider_id: openproxy_types::ProviderId::new("gemini"),
            account_id: None,
            model_row_id: None,
            sub_combo_id: None,
            priority_order: 0,
            weight: 100,
            active: true,
            rate_limit_scope: openproxy_types::RateLimitScope::Account,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            thinking_effort: None,
            description: None,
        },
        model: openproxy_types::Model {
            row_id: openproxy_types::ModelRowId(1),
            provider_id: openproxy_types::ProviderId::new("gemini"),
            model_id: openproxy_types::ModelId::new("gemma-4-26b-a4b-it"),
            target_format: openproxy_types::TargetFormat::Gemini,
            discovered_at: openproxy_types::now_unix_secs_str().into_boxed_str(),
            ..Default::default()
        },
        api_key: "k".to_string(),
        api_key_label: None,
        custom_meta: None,
    };

    let wrapped = adapter
        .wrap_request_body(
            body_bytes,
            openproxy_types::TargetFormat::Gemini,
            &openproxy_types::ModelId::new("gemma-4-26b-a4b-it"),
            &resolved_target,
        )
        .expect("wrap succeeds");
    let val: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();
    assert!(
        val["generationConfig"]["thinkingConfig"].is_null(),
        "thinkingConfig must be stripped for Gemma"
    );
}

#[test]
fn test_openai_to_gemini_max_completion_tokens_fallback() {
    let mut extra = serde_json::Map::new();
    extra.insert("max_completion_tokens".to_string(), json!(2048));
    let req = openproxy_types::OpenAIRequest {
        model: "gemini-2.5-flash".into(),
        max_tokens: None,
        extra,
        ..Default::default()
    };
    let gemini_req = openai_to_gemini_with_model(&req, &[], Some("gemini-2.5-flash"));
    assert_eq!(
        gemini_req
            .generation_config
            .as_ref()
            .unwrap()
            .max_output_tokens,
        Some(2048)
    );
}
