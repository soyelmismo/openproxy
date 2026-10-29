//! Outbound OpenAI → Anthropic Messages translation for media parts.
//!
//! Every provider that speaks the Messages API (Anthropic, MiniMax M3/M3.1,
//! Claude Code) receives its media as `{"type":"image"|"video"|"document",
//! "source":{...}}`. A part the translator does not recognise is forwarded
//! verbatim, which the upstream then rejects or silently ignores — the client
//! sees "I don't see an image" with a 200.

use crate::translation::*;
use openproxy_types::{OpenAIMessage, OpenAIRequest};
use serde_json::json;

fn request_with_parts(parts: serde_json::Value) -> OpenAIRequest {
    serde_json::from_value(json!({
        "model": "minimax-m3.1-flash",
        "messages": [{ "role": "user", "content": parts }]
    }))
    .expect("request deserializes")
}

fn converted_parts(parts: serde_json::Value) -> Vec<serde_json::Value> {
    let req = request_with_parts(parts);
    let anthropic = openai_to_anthropic(&req, "MiniMax-M3.1-Flash", &req.messages, false);
    let user = anthropic
        .messages
        .iter()
        .find(|m| m.role == "user")
        .expect("user message present");
    user.content
        .as_array()
        .expect("typed block array")
        .clone()
}

fn find_block<'a>(
    blocks: &'a [serde_json::Value],
    kind: &str,
) -> &'a serde_json::Value {
    blocks
        .iter()
        .find(|b| b["type"] == kind)
        .unwrap_or_else(|| panic!("expected a {kind} block, got {blocks:?}"))
}

#[test]
fn base64_image_becomes_anthropic_image_source() {
    let blocks = converted_parts(json!([
        { "type": "text", "text": "what is this" },
        {
            "type": "image_url",
            "image_url": { "url": "data:image/png;base64,AAAA" }
        }
    ]));
    let image = find_block(&blocks, "image");
    assert_eq!(image["source"]["type"], "base64");
    assert_eq!(image["source"]["media_type"], "image/png");
    assert_eq!(image["source"]["data"], "AAAA");
}

#[test]
fn remote_image_url_becomes_anthropic_url_source() {
    let blocks = converted_parts(json!([
        {
            "type": "image_url",
            "image_url": { "url": "https://example.com/a.png" }
        }
    ]));
    let image = find_block(&blocks, "image");
    assert_eq!(image["source"]["type"], "url");
    assert_eq!(image["source"]["url"], "https://example.com/a.png");
}

#[test]
fn video_part_becomes_anthropic_video_block() {
    let blocks = converted_parts(json!([
        {
            "type": "video_url",
            "video_url": { "url": "https://example.com/clip.mp4" }
        }
    ]));
    let video = find_block(&blocks, "video");
    assert_eq!(video["source"]["type"], "url");
    assert_eq!(video["source"]["url"], "https://example.com/clip.mp4");
}

#[test]
fn base64_video_part_becomes_base64_video_source() {
    let blocks = converted_parts(json!([
        {
            "type": "input_video",
            "video_url": { "url": "data:video/mp4;base64,ZZZZ" }
        }
    ]));
    let video = find_block(&blocks, "video");
    assert_eq!(video["source"]["type"], "base64");
    assert_eq!(video["source"]["media_type"], "video/mp4");
    assert_eq!(video["source"]["data"], "ZZZZ");
}

#[test]
fn file_part_becomes_anthropic_document_block() {
    let blocks = converted_parts(json!([
        {
            "type": "file",
            "file": { "file_data": "data:application/pdf;base64,PDFDATA" }
        }
    ]));
    let document = find_block(&blocks, "document");
    assert_eq!(document["source"]["type"], "base64");
    assert_eq!(document["source"]["media_type"], "application/pdf");
    assert_eq!(document["source"]["data"], "PDFDATA");
}

#[test]
fn file_id_source_becomes_mm_file_uri_round_trip() {
    // A provider file id survives as a URL-ish string so the outbound side can
    // rebuild the `file` source the upstream expects.
    let blocks = converted_parts(json!([
        {
            "type": "file",
            "file": { "file_id": "mm_file://abc123" }
        }
    ]));
    let document = find_block(&blocks, "document");
    assert_eq!(document["source"]["type"], "file");
    assert_eq!(document["source"]["file_id"], "abc123");
}

#[test]
fn inline_audio_becomes_base64_audio_source() {
    let blocks = converted_parts(json!([
        {
            "type": "input_audio",
            "input_audio": { "data": "AUDAUD", "format": "mp3" }
        }
    ]));
    let audio = find_block(&blocks, "audio");
    assert_eq!(audio["source"]["type"], "base64");
    assert_eq!(audio["source"]["media_type"], "audio/mp3");
    assert_eq!(audio["source"]["data"], "AUDAUD");
}

#[test]
fn text_parts_are_untouched() {
    let blocks = converted_parts(json!([{ "type": "text", "text": "hola" }]));
    let text = find_block(&blocks, "text");
    assert_eq!(text["text"], "hola");
}

#[test]
fn unknown_part_types_pass_through_unchanged() {
    let blocks = converted_parts(json!([{ "type": "mystery", "payload": 1 }]));
    assert_eq!(blocks[0]["type"], "mystery");
    assert_eq!(blocks[0]["payload"], 1);
}

#[test]
fn multiple_modalities_in_one_message_all_survive() {
    let blocks = converted_parts(json!([
        { "type": "text", "text": "compare these" },
        { "type": "image_url", "image_url": { "url": "data:image/png;base64,IMG" } },
        { "type": "video_url", "video_url": { "url": "https://example.com/v.mp4" } }
    ]));
    let kinds: Vec<&str> = blocks
        .iter()
        .filter_map(|b| b["type"].as_str())
        .collect();
    assert_eq!(kinds, vec!["text", "image", "video"]);
}

#[test]
fn anthropic_message_without_media_keeps_string_content() {
    let req = request_with_parts(json!("plain text"));
    let anthropic = openai_to_anthropic(&req, "MiniMax-M3.1-Flash", &req.messages, false);
    let user = anthropic
        .messages
        .iter()
        .find(|m| m.role == "user")
        .expect("user message present");
    assert_eq!(user.content.as_str(), Some("plain text"));
}

#[test]
fn system_message_still_extracted_separately() {
    let messages: Vec<OpenAIMessage> = serde_json::from_value(json!([
        { "role": "system", "content": "be brief" },
        {
            "role": "user",
            "content": [
                { "type": "text", "text": "hi" },
                { "type": "image_url", "image_url": { "url": "data:image/png;base64,AA" } }
            ]
        }
    ]))
    .expect("messages deserialize");
    let req = OpenAIRequest {
        model: "minimax-m3.1-flash".into(),
        messages: messages.clone(),
        stream: false,
        temperature: None,
        max_tokens: Some(64),
        top_p: None,
        stop: None,
        tools: None,
        tool_choice: None,
        top_k: None,
        user: None,
        extra: Default::default(),
    };
    let anthropic = openai_to_anthropic(&req, "MiniMax-M3.1-Flash", &messages, false);
    assert_eq!(anthropic.system.as_ref().and_then(|s| s.as_str()), Some("be brief"));
    let user = anthropic
        .messages
        .iter()
        .find(|m| m.role == "user")
        .expect("user message present");
    let parts = user.content.as_array().expect("typed block array");
    assert!(parts.iter().any(|p| p["type"] == "image"));
}
