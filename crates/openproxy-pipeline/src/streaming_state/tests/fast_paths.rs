use super::*;

fn normalized(normalizer: &mut ReasoningNormalizer, payload: &str) -> serde_json::Value {
    let action = normalizer.process_chunk(payload);
    match action {
        StreamAction::Passthrough => serde_json::from_str(payload).unwrap(),
        StreamAction::Mutate(modified) => serde_json::from_str(&modified).unwrap(),
        _ => panic!("unexpected action for a content/tool delta"),
    }
}

#[test]
fn tool_fast_path_initializes_state_before_cumulative_arguments() {
    let mut normalizer = ReasoningNormalizer::new();
    for (arguments, expected) in [
        ("{\"city\":", "{\"city\":"),
        ("{\"city\":\"Paris\"}", "\"Paris\"}"),
    ] {
        let payload = serde_json::json!({
            "choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": arguments}}]}}]
        }).to_string();
        let output = normalized(&mut normalizer, &payload);
        assert_eq!(
            output
                .pointer("/choices/0/delta/tool_calls/0/function/arguments")
                .unwrap(),
            expected
        );
    }
}

#[test]
fn test_pollinations_raw_chunk_extracts_think() {
    let raw = r#"{"id":"chatcmpl-6bce028001a9d52fe9c63dec","object":"chat.completion.chunk","created":1793678537,"model":"poe/GLM-5.3-Flash-T","choices":[{"index":0,"delta":{"content":"<think>\nLet me consider\n</think>\n\nHi there!"},"finish_reason":null}]}"#;
    let mut normalizer = ReasoningNormalizer::new();
    let act = normalizer.process_chunk(raw);
    let StreamAction::Mutate(s) = act else {
        panic!("expected mutate, got {act:?}");
    };
    assert!(s.contains("\"reasoning_content\":\"\\nLet me consider\\n\""));
    assert!(s.contains("\"content\":\"\\n\\nHi there!\""));
}

#[test]
fn ordinary_text_keeps_the_passthrough_fast_path() {
    let payload = r#"{"choices":[{"delta":{"content":"hola 🌍"}}]}"#;
    let mut normalizer = ReasoningNormalizer::new();
    assert!(matches!(
        normalizer.process_chunk(payload),
        StreamAction::Passthrough
    ));
    assert!(normalizer.finalize().is_none());
}

#[test]
fn reasoning_fast_path_cannot_skip_json_escaped_tags() {
    let mut normalizer = ReasoningNormalizer::new();
    let output = normalized(
        &mut normalizer,
        r#"{"choices":[{"delta":{"content":"\u003cthink\u003erazón\u003c/think\u003erespuesta"}}]}"#,
    );
    assert_eq!(
        output.pointer("/choices/0/delta/content").unwrap(),
        "respuesta"
    );
    assert_eq!(
        output
            .pointer("/choices/0/delta/reasoning_content")
            .unwrap(),
        "razón"
    );
}
