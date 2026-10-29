//! The media vocabulary contract.
//!
//! Each inbound endpoint speaks its own dialect. These tests pin the rule that
//! makes cross-endpoint routing safe: whatever dialect a media part arrives in,
//! it leaves the canonicaliser in exactly one shape, so every target formatter
//! only has to understand that shape.

use super::*;
use serde_json::json;

fn canon(part: serde_json::Value) -> serde_json::Value {
    canonical_media_part(&part).unwrap_or_else(|| panic!("part was dropped: {part}"))
}

#[test]
fn chat_completions_image_url_is_preserved() {
    let out = canon(json!({
        "type": "image_url",
        "image_url": {"url": "data:image/png;base64,QUJD", "detail": "high"}
    }));
    assert_eq!(out["type"], "image_url");
    assert_eq!(out["image_url"]["url"], "data:image/png;base64,QUJD");
    assert_eq!(out["image_url"]["detail"], "high");
}

#[test]
fn responses_input_image_becomes_image_url() {
    let out = canon(json!({
        "type": "input_image",
        "image_url": "data:image/png;base64,QUJD"
    }));
    assert_eq!(out["type"], "image_url");
    assert_eq!(out["image_url"]["url"], "data:image/png;base64,QUJD");
}

#[test]
fn anthropic_image_block_becomes_image_url() {
    let out = canon(json!({
        "type": "image",
        "source": {"type": "base64", "media_type": "image/png", "data": "QUJD"}
    }));
    assert_eq!(out["type"], "image_url");
    assert_eq!(out["image_url"]["url"], "data:image/png;base64,QUJD");
}

#[test]
fn anthropic_image_url_source_passes_through() {
    let out = canon(json!({
        "type": "image",
        "source": {"type": "url", "url": "https://example.com/a.png"}
    }));
    assert_eq!(out["image_url"]["url"], "https://example.com/a.png");
}

#[test]
fn video_never_keeps_the_responses_spelling() {
    // `input_video` is not in the Responses API nor in Chat Completions: both
    // vLLM (Hetzner) and NIM reject it with a 400 and only accept `video_url`.
    let out = canon(json!({
        "type": "input_video",
        "video_url": {"url": "data:video/mp4;base64,QUJD"}
    }));
    assert_eq!(out["type"], "video_url");
    assert_eq!(out["video_url"]["url"], "data:video/mp4;base64,QUJD");
}

#[test]
fn anthropic_video_block_becomes_video_url() {
    let out = canon(json!({
        "type": "video",
        "source": {"type": "base64", "media_type": "video/mp4", "data": "QUJD"}
    }));
    assert_eq!(out["type"], "video_url");
    assert_eq!(out["video_url"]["url"], "data:video/mp4;base64,QUJD");
}

#[test]
fn anthropic_document_block_becomes_chat_file() {
    let out = canon(json!({
        "type": "document",
        "source": {"type": "base64", "media_type": "application/pdf", "data": "QUJD"}
    }));
    assert_eq!(out["type"], "file");
    assert_eq!(out["file"]["file_data"], "data:application/pdf;base64,QUJD");
    assert_eq!(out["file"]["filename"], "document.pdf");
}

#[test]
fn responses_input_file_becomes_chat_file() {
    let out = canon(json!({
        "type": "input_file",
        "filename": "doc.pdf",
        "file_data": "QUJD"
    }));
    assert_eq!(out["type"], "file");
    assert_eq!(out["file"]["filename"], "doc.pdf");
    assert_eq!(out["file"]["file_data"], "data:application/pdf;base64,QUJD");
}

#[test]
fn file_id_document_keeps_the_id() {
    let out = canon(json!({
        "type": "input_file",
        "filename": "doc.pdf",
        "file_id": "file-abc"
    }));
    assert_eq!(out["type"], "file");
    assert_eq!(out["file"]["file_id"], "file-abc");
}

#[test]
fn audio_keeps_the_chat_completions_shape() {
    let out = canon(json!({
        "type": "input_audio",
        "input_audio": {"data": "QUJD", "format": "wav"}
    }));
    assert_eq!(out["type"], "input_audio");
    assert_eq!(out["input_audio"]["data"], "QUJD");
    assert_eq!(out["input_audio"]["format"], "wav");
}

#[test]
fn text_types_collapse_to_text() {
    for typ in ["text", "input_text", "output_text"] {
        let out = canon(json!({"type": typ, "text": "hola"}));
        assert_eq!(out, json!({"type": "text", "text": "hola"}), "{typ}");
    }
}

#[test]
fn empty_text_and_unknown_types_are_dropped() {
    assert!(canonical_media_part(&json!({"type": "text", "text": ""})).is_none());
    assert!(canonical_media_part(&json!({"type": "thinking", "text": "x"})).is_none());
}

#[test]
fn order_is_preserved_across_a_mixed_message() {
    let parts = vec![
        json!({"type": "text", "text": "look"}),
        json!({"type": "input_image", "image_url": "data:image/png;base64,QQ=="}),
        json!({"type": "input_video", "video_url": {"url": "data:video/mp4;base64,Qg=="}}),
        json!({"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "Qw=="}}),
    ];
    let out = canonical_media_parts(&parts);
    let types: Vec<&str> = out
        .iter()
        .map(|p| p["type"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(types, vec!["text", "image_url", "video_url", "file"]);
}

// ---------------------------------------------------------------------------
// Cross-endpoint matrix: the same media, arriving in every client dialect, must
// reach every target formatter as the same canonical part. This is the test
// that would have caught `input_image` vanishing on the Gemini path.
// ---------------------------------------------------------------------------

fn user_with(content: serde_json::Value) -> openproxy_types::OpenAIMessage {
    openproxy_types::OpenAIMessage {
        role: "user".into(),
        content: Some(content),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: Default::default(),
    }
}

fn normalized(content: serde_json::Value) -> serde_json::Value {
    let msg = user_with(content);
    let out = crate::formatting::normalize_openai_message(&msg);
    out.content.expect("content")
}

#[test]
fn responses_input_image_reaches_the_openai_formatter_as_image_url() {
    let content = normalized(json!([
        {"type": "input_text", "text": "number?"},
        {"type": "input_image", "image_url": "data:image/png;base64,QUJD"}
    ]));
    let parts = content.as_array().expect("array");
    assert_eq!(parts[0], json!({"type": "text", "text": "number?"}));
    assert_eq!(parts[1]["type"], "image_url");
    assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,QUJD");
}

#[test]
fn responses_input_image_reaches_gemini_as_inline_data() {
    // The bug: the part stayed `input_image`, Gemini's parser did not know the
    // type, and the request went out with the image silently missing.
    let content = normalized(json!([
        {"type": "input_text", "text": "number?"},
        {"type": "input_image", "image_url": "data:image/png;base64,QUJD"}
    ]));
    let parts = content.as_array().expect("array");
    let inline =
        openproxy_adapters::adapters::gemini::media::parse_media_part_to_inline_data(&parts[1])
            .expect("Gemini must see the image");
    assert_eq!(inline.mime_type, "image/png");
    assert_eq!(inline.data, "QUJD");
}

#[test]
fn chat_file_part_reaches_gemini_as_inline_pdf() {
    let content = normalized(json!([
        {"type": "text", "text": "code?"},
        {"type": "file", "file": {"filename": "d.pdf", "file_data": "data:application/pdf;base64,QUJD"}}
    ]));
    let parts = content.as_array().expect("array");
    let inline =
        openproxy_adapters::adapters::gemini::media::parse_media_part_to_inline_data(&parts[1])
            .expect("Gemini must see the document");
    assert_eq!(inline.mime_type, "application/pdf");
    assert_eq!(inline.data, "QUJD");
}

#[test]
fn anthropic_document_reaches_gemini_as_inline_pdf() {
    let content = normalized(json!([
        {"type": "text", "text": "code?"},
        {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "QUJD"}}
    ]));
    let parts = content.as_array().expect("array");
    let inline =
        openproxy_adapters::adapters::gemini::media::parse_media_part_to_inline_data(&parts[1])
            .expect("Gemini must see the document");
    assert_eq!(inline.mime_type, "application/pdf");
}

#[test]
fn video_reaches_gemini_as_inline_video() {
    let content = normalized(json!([
        {"type": "text", "text": "text?"},
        {"type": "input_video", "video_url": {"url": "data:video/mp4;base64,QUJD"}}
    ]));
    let parts = content.as_array().expect("array");
    let inline =
        openproxy_adapters::adapters::gemini::media::parse_media_part_to_inline_data(&parts[1])
            .expect("Gemini must see the video");
    assert_eq!(inline.mime_type, "video/mp4");
    assert_eq!(inline.data, "QUJD");
}

#[test]
fn canonical_part_reaches_the_anthropic_formatter_as_a_document_block() {
    let content = normalized(json!([
        {"type": "text", "text": "code?"},
        {"type": "input_file", "filename": "d.pdf", "file_data": "data:application/pdf;base64,QUJD"}
    ]));
    let msg = user_with(content);
    let req = openproxy_types::OpenAIRequest {
        model: "m".into(),
        messages: vec![msg],
        ..Default::default()
    };
    let anthro = crate::translation::openai_to_anthropic(&req, "m", &req.messages, false);
    let blocks = anthro.messages[0].content.as_array().expect("blocks");
    let has_document = blocks
        .iter()
        .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("document"));
    assert!(has_document, "document block missing: {blocks:?}");
}

#[test]
fn a_media_message_always_requests_normalization() {
    // Regression guard: `message_needs_openai_normalization` used to return
    // false when media was present, which is precisely backwards — a
    // multimodal message is the one that most needs its dialect rewritten.
    let msg = user_with(json!([
        {"type": "input_image", "image_url": "data:image/png;base64,QUJD"}
    ]));
    assert!(
        crate::formatting::message_needs_openai_normalization(&msg),
        "a message carrying media must be normalized"
    );
}

#[test]
fn a_text_only_array_message_also_requests_normalization() {
    let msg = user_with(json!([{"type": "input_text", "text": "hola"}]));
    assert!(crate::formatting::message_needs_openai_normalization(&msg));
}
