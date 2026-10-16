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
    let parsed: serde_json::Value = serde_json::from_str(&sanitized).expect("must be valid JSON");
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

#[test]
fn reasoning_and_reasoning_content_collapse_to_the_standard_field() {
    // NVIDIA NIM rechaza el cuerpo con un 400 de campo duplicado cuando el
    // cliente reenvía ambos campos en el historial del asistente.
    let mut extra = serde_json::Map::new();
    extra.insert("reasoning".to_string(), json!("paso a paso"));
    extra.insert("reasoning_content".to_string(), json!("paso a paso"));
    let msg = OpenAIMessage {
        role: "assistant".to_string(),
        content: Some(json!("respuesta")),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra,
    };

    assert!(message_needs_openai_normalization(&msg));
    let normalized = normalize_openai_message(&msg);

    assert!(
        normalized.extra.contains_key("reasoning_content"),
        "reasoning_content debe conservarse, es el nombre estándar"
    );
    assert_eq!(
        normalized.extra["reasoning_content"],
        json!("paso a paso"),
        "el contenido no debe alterarse"
    );
    assert!(
        !normalized.extra.contains_key("reasoning"),
        "reasoning debe descartarse para no duplicar el campo"
    );

    // El cuerpo resultante debe deserializar sin choque de campos.
    let body = serde_json::to_value(&normalized).expect("serializa");
    let obj = body.as_object().expect("objeto");
    assert!(!obj.contains_key("reasoning"));
}

#[test]
fn lone_reasoning_survives_for_providers_that_only_know_that_name() {
    let mut extra = serde_json::Map::new();
    extra.insert("reasoning".to_string(), json!("sin duplicar"));
    let msg = OpenAIMessage {
        role: "assistant".to_string(),
        content: Some(json!("respuesta")),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra,
    };

    assert!(
        !message_needs_openai_normalization(&msg),
        "un reasoning suelto no debe forzar normalización"
    );

    let normalized = normalize_openai_message(&msg);
    assert!(
        normalized.extra.contains_key("reasoning"),
        "un reasoning suelto se deja pasar: hay proveedores que solo usan ese nombre"
    );
}

#[test]
fn empty_or_whitespace_assistant_content_normalized_to_none() {
    // NVIDIA NIM (especialmente diffusiongemma) rechaza 'content: ""' o 'content: "   "'
    // en mensajes de rol assistant con 400 'Empty content is not allowed for assistant messages'.
    // Debe normalizarse a None (que serializa como null en JSON).
    let msg_empty = OpenAIMessage {
        role: "assistant".to_string(),
        content: Some(json!("")),
        name: None,
        tool_call_id: None,
        tool_calls: Some(vec![json!({
            "id": "call_1",
            "type": "function",
            "function": {"name": "test", "arguments": "{}"}
        })]),
        extra: Default::default(),
    };

    assert!(message_needs_openai_normalization(&msg_empty));
    let norm_empty = normalize_openai_message(&msg_empty);
    assert_eq!(norm_empty.content, None);

    let val = serde_json::to_value(&norm_empty).expect("serializa");
    assert_eq!(val["content"], serde_json::Value::Null);

    let msg_ws = OpenAIMessage {
        role: "assistant".to_string(),
        content: Some(json!("   \n\t  ")),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: Default::default(),
    };

    assert!(message_needs_openai_normalization(&msg_ws));
    let norm_ws = normalize_openai_message(&msg_ws);
    assert_eq!(norm_ws.content, None);

    // Contenido no vacío no debe ser alterado
    let msg_valid = OpenAIMessage {
        role: "assistant".to_string(),
        content: Some(json!("hola mundo")),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: Default::default(),
    };
    let norm_valid = normalize_openai_message(&msg_valid);
    assert_eq!(norm_valid.content, Some(json!("hola mundo")));
}
