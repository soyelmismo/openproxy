//! Regression test for the Gemini probe struct's handling of the
//! `role` sibling field inside `content`.
//!
//! Real-world Gemini chunks include `"role":"model"` inside `content`,
//! alongside `parts`. The probe must skip this unknown field without
//! erroring. The existing sse::tests did not cover this case because
//! their test fixtures omitted `role`.

use openproxy_pipeline::sse::parse_gemini_sse_line;

#[test]
fn gemini_probe_handles_role_sibling_in_content() {
    // Real-world Gemini chunk: `content` has BOTH `parts` and `role`.
    // The probe's `GeminiContentProbe` only declares `parts`, so serde
    // must skip `role` (which it does by default for unknown fields).
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
    // Text containing a comma — ensures the probe doesn't misparse
    // commas inside JSON string values.
    let line = r#"data: {"candidates":[{"content":{"parts":[{"text":", "}],"role":"model"}}]}"#;
    let chunk = parse_gemini_sse_line(line, "test-id", 0, "gemini-pro")
        .expect("probe should handle comma text")
        .expect("probe should return a chunk");
    let content = chunk.payload["choices"][0]["delta"]["content"]
        .as_str()
        .expect("content should be extracted");
    assert_eq!(content, ", ");
}
