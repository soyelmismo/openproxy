use super::*;
use serde_json::json;

#[test]
fn test_sanitize_tool_call_arguments_valid_json() {
    let valid = r#"{"AbsolutePath":"/root/proyectos/openproxy/file.rs"}"#;
    assert_eq!(sanitize_tool_call_arguments(valid), valid);

    let empty_obj = "{}";
    assert_eq!(sanitize_tool_call_arguments(empty_obj), empty_obj);
}

#[test]
fn test_sanitize_tool_call_arguments_trailing_characters() {
    // Exact symptom seen in NVIDIA NIM and Antseed logs:
    // trailing characters after valid JSON document at col 7186 / char 7185
    let trailing = r#"{"AbsolutePath":"/root/file.rs"} trailing extra data after json"#;
    let sanitized = sanitize_tool_call_arguments(trailing);
    let parsed: serde_json::Value =
        serde_json::from_str(&sanitized).expect("must be valid JSON");
    assert_eq!(parsed["AbsolutePath"], "/root/file.rs");
    assert!(!sanitized.contains("trailing extra data"));
}

#[test]
fn test_sanitize_tool_call_arguments_unterminated_string() {
    // Exact symptom seen in Dahl logs:
    // "Unterminated string starting at: line 1 column 13 (char 12)"
    let unterminated = r#"{"message": "incomplete text"#;
    let sanitized = sanitize_tool_call_arguments(unterminated);
    let parsed: serde_json::Value =
        serde_json::from_str(&sanitized).expect("must be valid JSON after repair");
    assert_eq!(parsed["message"], "incomplete text");
}

#[test]
fn test_sanitize_tool_call_arguments_empty_and_garbage() {
    assert_eq!(sanitize_tool_call_arguments(""), "{}");
    assert_eq!(sanitize_tool_call_arguments("   "), "{}");
    assert_eq!(sanitize_tool_call_arguments("not a json at all"), "{}");
}

#[test]
fn test_openai_formatter_sanitizes_malformed_tool_calls_in_history() {
    let malformed_args = r#"{"file":"test.rs"} extra characters at line 1 column 7186"#;
    let msg = OpenAIMessage {
        role: "assistant".into(),
        content: None,
        name: None,
        tool_call_id: None,
        tool_calls: Some(vec![json!({
            "id": "call_abc123",
            "type": "function",
            "function": {
                "name": "edit_file",
                "arguments": malformed_args
            }
        })]),
        extra: Default::default(),
    };

    assert!(message_needs_openai_normalization(&msg));
    let normalized = normalize_openai_message(&msg);
    let tc = &normalized.tool_calls.as_ref().unwrap()[0];
    let clean_args = tc["function"]["arguments"].as_str().unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(clean_args).expect("arguments must now be strictly valid JSON");
    assert_eq!(parsed["file"], "test.rs");
}
