//! Inbound Anthropic Messages → internal OpenAI request.
//!
//! The Messages API carries pixels as `{"type":"image","source":{...}}`, which
//! is not the shape the rest of the pipeline speaks. Dropping those blocks
//! leaves the request text-only, and the upstream then answers "I don't see an
//! image attached" even though the client sent one.

use crate::translation::*;
use serde_json::json;

fn request_with_content(content: serde_json::Value) -> AnthropicRequest {
    serde_json::from_value(json!({
        "model": "claude-opus-5-5",
        "max_tokens": 64,
        "messages": [{ "role": "user", "content": content }]
    }))
    .expect("request deserializes")
}

fn sole_user_message(req: &openproxy_types::OpenAIRequest) -> &openproxy_types::OpenAIMessage {
    req.messages
        .iter()
        .find(|m| m.role == "user")
        .expect("user message present")
}

#[test]
fn anthropic_base64_image_becomes_image_url_part() {
    let req = request_with_content(json!([
        { "type": "text", "text": "What number is this?" },
        {
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": "iVBORw0KGgo="
            }
        }
    ]));

    let openai_req = anthropic_request_to_openai(req);
    let msg = sole_user_message(&openai_req);
    let parts = msg
        .content
        .as_ref()
        .and_then(|c| c.as_array())
        .expect("image requests keep a typed part array");

    let image = parts
        .iter()
        .find(|p| p["type"] == "image_url")
        .expect("image_url part survives the conversion");
    assert_eq!(
        image["image_url"]["url"],
        "data:image/png;base64,iVBORw0KGgo="
    );
}

#[test]
fn anthropic_url_image_becomes_image_url_part() {
    let req = request_with_content(json!([
        {
            "type": "image",
            "source": { "type": "url", "url": "https://example.com/cat.png" }
        }
    ]));

    let openai_req = anthropic_request_to_openai(req);
    let msg = sole_user_message(&openai_req);
    let parts = msg
        .content
        .as_ref()
        .and_then(|c| c.as_array())
        .expect("typed part array");

    let image = parts
        .iter()
        .find(|p| p["type"] == "image_url")
        .expect("image_url part survives the conversion");
    assert_eq!(image["image_url"]["url"], "https://example.com/cat.png");
}

#[test]
fn text_only_anthropic_message_stays_a_plain_string() {
    let req = request_with_content(json!([{ "type": "text", "text": "hello" }]));

    let openai_req = anthropic_request_to_openai(req);
    let msg = sole_user_message(&openai_req);
    assert_eq!(
        msg.content.as_ref().and_then(|c| c.as_str()),
        Some("hello"),
        "no images means no array; downstream text handling expects a string"
    );
}

#[test]
fn image_without_accompanying_text_still_produces_a_message() {
    let req = request_with_content(json!([
        {
            "type": "image",
            "source": { "type": "base64", "media_type": "image/jpeg", "data": "AAAA" }
        }
    ]));

    let openai_req = anthropic_request_to_openai(req);
    let msg = sole_user_message(&openai_req);
    let parts = msg
        .content
        .as_ref()
        .and_then(|c| c.as_array())
        .expect("typed part array");
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0]["type"], "image_url");
}

#[test]
fn image_block_with_unknown_source_is_dropped_not_stringified() {
    let req = request_with_content(json!([
        { "type": "text", "text": "hello" },
        { "type": "image", "source": { "type": "file_id", "file_id": "abc" } }
    ]));

    let openai_req = anthropic_request_to_openai(req);
    let msg = sole_user_message(&openai_req);
    assert_eq!(
        msg.content.as_ref().and_then(|c| c.as_str()),
        Some("hello"),
        "an unsupported source must not leak a JSON blob into the prompt text"
    );
}

#[test]
fn openai_style_image_url_inside_messages_passes_through() {
    let req = request_with_content(json!([
        { "type": "text", "text": "look" },
        {
            "type": "image_url",
            "image_url": { "url": "data:image/png;base64,ZZZ" }
        }
    ]));

    let openai_req = anthropic_request_to_openai(req);
    let msg = sole_user_message(&openai_req);
    let parts = msg
        .content
        .as_ref()
        .and_then(|c| c.as_array())
        .expect("typed part array");
    let image = parts
        .iter()
        .find(|p| p["type"] == "image_url")
        .expect("image_url part survives");
    assert_eq!(image["image_url"]["url"], "data:image/png;base64,ZZZ");
}
