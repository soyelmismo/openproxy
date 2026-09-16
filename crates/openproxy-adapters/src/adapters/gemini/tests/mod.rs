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
                    },
                    GeminiPart {
                        text: Some("world!".to_string()),
                        inline_data: None,
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
