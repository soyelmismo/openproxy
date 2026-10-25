#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Regression test for the Gemini probe struct's handling of the
//! `role` sibling field inside `content`.
//!
//! Real-world Gemini chunks carry `"role":"model"` inside `content` next to
//! `parts`, an unknown field that `GeminiContentProbe` must skip without erroring.

use openproxy_pipeline::sse::parse_gemini_sse_line_test as parse_gemini_sse_line;

#[test]
fn gemini_probe_handles_role_sibling_in_content() {
    // `content` carries both `parts` and the unknown `role` field
    let line = r#"data: {"candidates":[{"content":{"parts":[{"text":"Hello"}],"role":"model"}}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":1,"totalTokenCount":11}}"#;
    let chunk = parse_gemini_sse_line(line, "test-id", 0, "gemini-pro")
        .expect("probe should handle `role` sibling field")
        .expect("probe should return a chunk");
    let content = chunk.payload["choices"][0]["delta"]["content"]
        .as_str()
        .expect("content should be extracted");
    assert_eq!(content, "Hello");
    let usage = chunk.usage.expect("usage should be extracted");
    assert_eq!(usage.prompt_tokens, 10);
    assert_eq!(usage.completion_tokens, 1);
    assert_eq!(usage.total_tokens, 11);
}

#[test]
fn gemini_probe_handles_role_with_comma_text() {
    // a comma inside a JSON string value must not shift the parse
    let line = r#"data: {"candidates":[{"content":{"parts":[{"text":", "}],"role":"model"}}]}"#;
    let chunk = parse_gemini_sse_line(line, "test-id", 0, "gemini-pro")
        .expect("probe should handle comma text")
        .expect("probe should return a chunk");
    let content = chunk.payload["choices"][0]["delta"]["content"]
        .as_str()
        .expect("content should be extracted");
    assert_eq!(content, ", ");
}
