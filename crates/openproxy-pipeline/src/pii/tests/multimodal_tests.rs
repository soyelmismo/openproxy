use super::super::*;
use openproxy_types::config::PiiEntity;
use openproxy_types::message::OpenAIMessage;

#[test]
fn test_multimodal_image_url_base64_payload_preserved_without_corruption() {
    let engine = PiiEngine::new(&[PiiEntity::Person, PiiEntity::Email, PiiEntity::Secret]);
    let mut session = PiiSession::new(true);

    // Realistic JPEG header and random-looking base64 that would previously trigger Secret entropy detection
    let raw_base64 = "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDABALDA4MChAODQ4SERATGCgaGBYWGDEjJR0oOjM9PDkzODdASFxOQERXRTc4UG1RV19iZ2hnPk1xeXBkeFxlZ2P/2wBDARESEhgVGC8aGi9jQjhCY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2P/";
    let data_uri = format!("data:image/jpeg;base64,{raw_base64}");

    let msg = OpenAIMessage {
        role: "user".to_string(),
        name: None,
        content: Some(serde_json::json!([
            {
                "type": "text",
                "text": "Please analyze this image for John Doe (john.doe@example.com)"
            },
            {
                "type": "image_url",
                "image_url": {
                    "url": &data_uri,
                    "detail": "high"
                }
            }
        ])),
        tool_calls: None,
        tool_call_id: None,
        extra: serde_json::Map::new(),
    };

    let redacted_msgs = engine.redact_messages(&[msg], &mut session);
    assert_eq!(redacted_msgs.len(), 1);

    let content_arr = redacted_msgs[0]
        .content
        .as_ref()
        .unwrap()
        .as_array()
        .unwrap();
    let text_part = &content_arr[0];
    let image_part = &content_arr[1];

    // 1. Text is properly pseudonymized
    let text = text_part["text"].as_str().unwrap();
    assert!(!text.contains("John Doe"));
    assert!(!text.contains("john.doe@example.com"));
    assert_eq!(
        session.restore_text(text),
        "Please analyze this image for John Doe (john.doe@example.com)"
    );

    // 2. Multimodal base64 data URI is 100% UNTOUCHED and BIT-PERFECT
    let url = image_part["image_url"]["url"].as_str().unwrap();
    assert_eq!(url, data_uri);
    assert!(!url.contains("sec_"));

    // 3. No secrets falsely detected in the base64 image
    let secret_count = session.counts.get(&PiiEntity::Secret).copied().unwrap_or(0);
    assert_eq!(
        secret_count, 0,
        "Base64 image data must not trigger false secret redactions"
    );
}

#[test]
fn test_multimodal_gemini_inline_data_and_anthropic_source_preserved() {
    let engine = PiiEngine::new(&[PiiEntity::Secret, PiiEntity::Person]);
    let mut session = PiiSession::new(true);

    let raw_base64 = "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDABALDA4MChAODQ4SERATGCgaGBYWGDEjJR0oOjM9PDkzODdASFxOQERXRTc4UG1RV19iZ2hnPk1xeXBkeFxlZ2P/2wBDARESEhgVGC8aGi9jQjhCY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2P/";

    // Gemini shape: contents[].parts[].inline_data.data
    let mut gemini_val = serde_json::json!({
        "contents": [{
            "parts": [
                {"text": "Hello John Doe"},
                {"inline_data": {"mime_type": "image/jpeg", "data": raw_base64}}
            ]
        }]
    });

    engine.redact_json_value(&mut gemini_val, &mut session);

    let parts = &gemini_val["contents"][0]["parts"];
    assert!(!parts[0]["text"].as_str().unwrap().contains("John Doe"));
    assert_eq!(
        parts[1]["inline_data"]["data"].as_str().unwrap(),
        raw_base64
    );

    // Anthropic shape: messages[].content[].source.data
    let mut anthropic_val = serde_json::json!({
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "Photo of John Doe"},
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/jpeg",
                        "data": raw_base64
                    }
                }
            ]
        }]
    });

    engine.redact_json_value(&mut anthropic_val, &mut session);
    let anthropic_content = &anthropic_val["messages"][0]["content"];
    assert!(
        !anthropic_content[0]["text"]
            .as_str()
            .unwrap()
            .contains("John Doe")
    );
    assert_eq!(
        anthropic_content[1]["source"]["data"].as_str().unwrap(),
        raw_base64
    );
}

#[test]
fn test_data_uri_in_markdown_text_protected() {
    let engine = PiiEngine::new(&[PiiEntity::Person, PiiEntity::Secret]);
    let mut session = PiiSession::new(true);

    let data_uri = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
    let input = format!("Here is the chart: ![chart]({data_uri}) prepared by John Doe.");

    let redacted = engine.redact_text(&input, &mut session);
    assert!(!redacted.contains("John Doe"));
    assert!(
        redacted.contains(data_uri),
        "Embedded Data URI must remain uncorrupted"
    );
    assert_eq!(session.restore_text(&redacted), input);
}
