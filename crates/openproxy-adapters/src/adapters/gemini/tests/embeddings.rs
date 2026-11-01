//! End-to-end tests of the multimodal embeddings path, from the JSON a client
//! sends to the bytes that go on the wire to Gemini.
//!
//! These are the tests that would have caught the two bugs the unit tests found
//! while the layer was being built: plain text being rejected as if it were
//! pre-tokenized ids, and a document part being missing from the translation.

use super::super::embeddings::{
    deserialize_gemini_embedding_response, serialize_gemini_embedding_request,
};
use openproxy_types::embeddings::EmbeddingRequest;

/// The whole chain a real request travels: client dialect in, Gemini wire bytes
/// out, Gemini response in, canonical response out.
#[test]
fn a_client_request_becomes_the_gemini_wire_body() {
    // A client speaking the OpenAI dialect, sending text plus an image in one
    // item and a second item that is text only.
    let client_json = r#"{
        "model": "gemini-embedding-2",
        "input": [
            [
                {"type": "text", "text": "what is in this chart"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}}
            ],
            "a plain second item"
        ],
        "dimensions": 768
    }"#;

    let req: EmbeddingRequest = serde_json::from_str(client_json).expect("client json parses");
    assert_eq!(req.input.items().len(), 2);

    let body = serialize_gemini_embedding_request(&req, "gemini-embedding-2").expect("translate");
    let wire: serde_json::Value = serde_json::from_slice(&body).expect("wire is json");

    let requests = wire["requests"].as_array().expect("batch envelope");
    assert_eq!(requests.len(), 2);

    // The mixed item keeps its part order and both modalities survive.
    let first = requests[0]["content"]["parts"].as_array().unwrap();
    assert_eq!(first.len(), 2);
    assert_eq!(first[0]["text"], "what is in this chart");
    assert_eq!(first[1]["inlineData"]["mimeType"], "image/png");
    assert_eq!(first[1]["inlineData"]["data"], "iVBORw0KGgo=");

    // The plain item is a single text part, not an error.
    let second = requests[1]["content"]["parts"].as_array().unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0]["text"], "a plain second item");

    // Both carry the retrieval-query task and the requested dimensionality.
    for request in requests {
        assert_eq!(request["taskType"], "RETRIEVAL_QUERY");
        assert_eq!(request["outputDimensionality"], 768);
    }
}

/// The same request with a `-doc` alias flips the task side, which is how the
/// corpus and query halves of an asymmetric encoder are told apart.
#[test]
fn a_doc_alias_flips_the_task_side_over_the_wire() {
    let client_json = r#"{"model":"gemini-embedding-2-doc","input":"corpus text"}"#;
    let req: EmbeddingRequest = serde_json::from_str(client_json).expect("parses");
    let body =
        serialize_gemini_embedding_request(&req, "gemini-embedding-2-doc").expect("translate");
    let wire: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(wire["requests"][0]["taskType"], "RETRIEVAL_DOCUMENT");
}

/// A client speaking the Gemini dialect directly is also understood, because
/// `inline_data` is one of the accepted part shapes.
#[test]
fn a_gemini_dialect_request_is_also_understood() {
    let client_json = r#"{
        "model": "gemini-embedding-2",
        "input": [
            {"inline_data": {"mime_type": "audio/mp3", "data": "SUQzBAAAAAAAI1RTU0U"}}
        ]
    }"#;
    let req: EmbeddingRequest = serde_json::from_str(client_json).expect("gemini dialect parses");
    let body = serialize_gemini_embedding_request(&req, "gemini-embedding-2").expect("translate");
    let wire: serde_json::Value = serde_json::from_slice(&body).expect("json");
    let part = &wire["requests"][0]["content"]["parts"][0];
    assert_eq!(part["inlineData"]["mimeType"], "audio/mp3");
    assert_eq!(part["inlineData"]["data"], "SUQzBAAAAAAAI1RTU0U");
}

/// The response side: Gemini's batch envelope becomes the canonical list, in
/// order, with the index each item is addressed by.
#[test]
fn a_gemini_response_becomes_the_canonical_response() {
    let upstream = serde_json::json!({
        "embeddings": [
            {"values": [0.5, -0.25, 0.125]},
            {"values": [1.0, 0.0, -1.0]}
        ]
    });
    let resp = deserialize_gemini_embedding_response(upstream).expect("translate");

    assert_eq!(resp.object, "list");
    assert_eq!(resp.data.len(), 2);
    assert_eq!(resp.data[0].index, 0);
    assert_eq!(resp.data[1].index, 1);
    let openproxy_types::embeddings::EmbeddingVector::Floats(first) = &resp.data[0].embedding
    else {
        panic!("expected floats");
    };
    assert_eq!(&**first, &[0.5, -0.25, 0.125]);
}

/// A remote reference cannot be translated, and the error says why rather than
/// shipping a URL where Gemini expects base64.
#[test]
fn a_remote_reference_fails_with_a_useful_message() {
    let client_json = r#"{
        "model": "gemini-embedding-2",
        "input": [{"type": "image_url", "image_url": {"url": "https://example.com/chart.png"}}]
    }"#;
    let req: EmbeddingRequest = serde_json::from_str(client_json).expect("parses");
    let err = serialize_gemini_embedding_request(&req, "gemini-embedding-2")
        .expect_err("remote media is not translatable");
    let msg = err.to_string();
    assert!(msg.contains("remote reference"), "{msg}");
    assert!(msg.contains("data: URL"), "{msg}");
}

/// The admin model tester pokes an endpoint by hand instead of going through
/// the embeddings dispatch. It builds an OpenAI-shaped body, runs it through
/// `wrap_request_body` and posts it to `build_embeddings_url`.
///
/// `wrap_request_body` is a chat-shaped patch (contents, generationConfig,
/// thought signatures). For embeddings it is a no-op, so the body that reaches
/// Gemini is still OpenAI dialect while the URL is Gemini dialect. This test
/// pins that gap so it cannot pass silently: the URL the tester builds is
/// correct, the body it sends is not.
#[test]
fn the_tester_path_sends_the_wrong_body_to_gemini() {
    use super::super::GeminiAdapter;
    use crate::adapters::traits::ProviderAdapter;

    let adapter = GeminiAdapter::new();
    let model_id = openproxy_types::ids::ModelId::new("gemini-embedding-2".to_string());
    let target = openproxy_types::TargetFormat::Gemini;
    let dummy_target =
        openproxy_types::context::ResolvedTarget::test("gemini", "gemini-embedding-2");

    // What `build_audio_or_specialized_payload` builds with is_embedding = true.
    let tester_body = serde_json::json!({
        "model": "gemini-embedding-2",
        "input": "hello"
    });
    let tester_url = adapter.build_embeddings_url(&model_id);

    // The URL is right.
    assert!(
        tester_url.ends_with(":batchEmbedContents"),
        "url was {tester_url}"
    );

    // The body is not. `wrap_request_body` leaves an embeddings body alone.
    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from(serde_json::to_vec(&tester_body).unwrap()),
            target,
            &model_id,
            &dummy_target,
        )
        .expect("wrapping does not fail");
    let wrapped_json: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();

    // Still OpenAI dialect: `input`, no `requests`.
    assert_eq!(wrapped_json["input"], "hello");
    assert!(
        wrapped_json.get("requests").is_none(),
        "wrap_request_body translated an embeddings body, this test can be deleted"
    );

    // The dispatch path does translate it. That is the difference.
    let req: EmbeddingRequest =
        serde_json::from_value(tester_body).expect("the tester body is a valid request");
    let dispatched = adapter
        .format_embedding_request(target, &req, "gemini-embedding-2")
        .expect("translates");
    let dispatched_json: serde_json::Value = serde_json::from_slice(&dispatched).unwrap();
    assert!(
        dispatched_json.get("requests").is_some(),
        "the dispatch path must produce the Gemini wire shape"
    );
}
