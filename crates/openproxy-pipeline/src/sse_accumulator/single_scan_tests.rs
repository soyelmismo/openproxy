use super::*;

#[test]
fn first_non_string_field_wins_and_truncated_strings_terminate() {
    for value in ["null", "7", "true", "{}", "[]"] {
        let payload = format!(
            r#"{{"choices":[{{"delta":{{"content":{value},"content":"ignored","reasoning_content":"kept"}}}}]}}"#
        );
        let fields = extract_delta_fields(&payload);
        assert_eq!(fields.content, None);
        assert_eq!(fields.reasoning_content, Some("kept"));
    }
    for payload in [
        r#"{"choices":[{"delta":{"content":"unfinished"#,
        r#"{"choices":[{"delta":{"content":"unfinished\"#,
    ] {
        assert_eq!(extract_delta_fields(payload).content, None);
    }
}

#[test]
fn combined_scan_matches_decoded_fields_for_varied_valid_json() {
    let texts = [
        "",
        "plain",
        "ñ🦀中文",
        "quote\" and \\ slash",
        "\n\t\r",
        "<think>",
        "content reasoning_content",
    ];
    for content in texts {
        for reasoning in texts {
            for reverse in [false, true] {
                let (c, r) = (
                    serde_json::to_string(content).unwrap(),
                    serde_json::to_string(reasoning).unwrap(),
                );
                let fields_json = if reverse {
                    format!(r#""reasoning_content":{r},"content":{c}"#)
                } else {
                    format!(r#""content":{c},"reasoning_content":{r}"#)
                };
                let payload = format!(
                    r#"{{"choices":[{{"delta":{{"nested":[{{"content":"ignored","reasoning_content":"ignored"}}],{fields_json}}}}}]}}"#
                );
                let fields = extract_delta_fields(&payload);
                for (raw, expected) in [
                    (fields.content, content),
                    (fields.reasoning_content, reasoning),
                ] {
                    let mut decoded = Vec::new();
                    decode_json_escape_into(raw.unwrap(), &mut decoded);
                    assert_eq!(String::from_utf8(decoded).unwrap(), expected);
                }
            }
        }
    }
}

#[test]
fn combined_scan_preserves_tool_before_reasoning_at_byte_cap() {
    use crate::streaming::StreamingChunkStage;
    let mut acc = ResponseAccumulator::new();
    let fill = "a".repeat(MAX_ACCUMULATED_BYTES - 4);
    acc.append_openai_raw(&format!(
        r#"{{"choices":[{{"delta":{{"content":"{fill}"}}}}]}}"#
    ));
    acc.process_chunk(r#"{"choices":[{"delta":{"reasoning_content":"think","tool_calls":[{"index":0,"id":"x","function":{"name":"f","arguments":"{}"}}]}}]}"#);
    let output = acc.finish("id", 0, "model");
    assert_eq!(
        output["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
        "{}"
    );
    assert!(
        output["choices"][0]["message"]
            .get("reasoning_content")
            .is_none()
    );
    assert!(acc.is_truncated());
}

#[test]
fn single_scan_extract_delta_fields_both_present() {
    let payload = r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":"Hello world","reasoning_content":"Thinking process"}}]}"#;
    let fields = extract_delta_fields(payload);
    assert_eq!(fields.content, Some("Hello world"));
    assert_eq!(fields.reasoning_content, Some("Thinking process"));
}

#[test]
fn single_scan_extract_delta_fields_escaped_reasoning_and_surrogates() {
    let payload = r#"{"id":"chatcmpl-2","choices":[{"index":0,"delta":{"reasoning_content":"Step 1: \u003cthink\u003e \uD83D\uDE00 \"quotes\" and \\ slashes","content":"Answer: 42 \n newline"}}]}"#;
    let fields = extract_delta_fields(payload);
    assert_eq!(
        fields.reasoning_content,
        Some(r#"Step 1: \u003cthink\u003e \uD83D\uDE00 \"quotes\" and \\ slashes"#)
    );
    assert_eq!(fields.content, Some(r"Answer: 42 \n newline"));

    // Verify accumulator decoding under process_chunk stage
    use crate::streaming::StreamingChunkStage;
    let mut acc = ResponseAccumulator::new();
    acc.process_chunk(payload);
    let v = acc.finish("test-esc", 0, "model");
    let content = v["choices"][0]["message"]["content"].as_str().unwrap();
    let reasoning = v["choices"][0]["message"]["reasoning_content"]
        .as_str()
        .unwrap();
    assert_eq!(content, "Answer: 42 \n newline");
    assert_eq!(reasoning, "Step 1: <think> 😀 \"quotes\" and \\ slashes");
}

#[test]
fn single_scan_tool_call_delta_first_chunk_preservation() {
    use crate::streaming::StreamingChunkStage;
    let mut acc = ResponseAccumulator::new();

    // First chunk carries the initial tool call with id, name, and first argument delta
    let chunk1 = r#"{"id":"chatcmpl-tool","choices":[{"index":0,"delta":{"role":"assistant","content":null,"tool_calls":[{"index":0,"id":"call_123","type":"function","function":{"name":"get_weather","arguments":"{\"loc"}}]},"finish_reason":null}]}"#;
    acc.process_chunk(chunk1);

    // Second chunk carries argument continuation
    let chunk2 = r#"{"id":"chatcmpl-tool","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ation\":\"Madrid\"}"}}]},"finish_reason":null}]}"#;
    acc.process_chunk(chunk2);

    let v = acc.finish("id", 0, "m");
    assert_eq!(v["choices"][0]["message"]["content"], Value::Null);
    let tc = &v["choices"][0]["message"]["tool_calls"].as_array().unwrap()[0];
    assert_eq!(tc["id"], "call_123");
    assert_eq!(tc["function"]["name"], "get_weather");
    assert_eq!(tc["function"]["arguments"], r#"{"location":"Madrid"}"#);
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
}

#[test]
fn single_scan_nested_tool_call_and_pii_unicode_boundaries() {
    use crate::streaming::StreamingChunkStage;
    let mut acc = ResponseAccumulator::new();

    // Nested JSON inside arguments with Spanish/PII characters (email, DNI pattern, accents)
    let chunk = r#"{"choices":[{"delta":{"content":"Usuario: Juan Pérez\nEmail: juan.perez@example.com","tool_calls":[{"index":0,"id":"call_pii","function":{"name":"verify","arguments":"{\"query\":\"DNI 12345678Z señor niño 🦀\"}"}}]}}]}"#;
    acc.process_chunk(chunk);

    let v = acc.finish("id", 0, "m");
    let content = v["choices"][0]["message"]["content"].as_str().unwrap();
    assert_eq!(
        content,
        "Usuario: Juan Pérez\nEmail: juan.perez@example.com"
    );

    let tc = &v["choices"][0]["message"]["tool_calls"].as_array().unwrap()[0];
    assert_eq!(tc["id"], "call_pii");
    assert_eq!(
        tc["function"]["arguments"],
        "{\"query\":\"DNI 12345678Z señor niño 🦀\"}"
    );
}

#[test]
fn single_scan_strict_choices0_delta_boundaries() {
    // Other choices or nested delta lookalikes outside choices[0].delta must NOT pollute
    let chunk = r#"{"id":"x","choices":[{"index":0,"delta":{"content":"choice0 content"}},{"index":1,"delta":{"content":"choice1 content"}}]}"#;
    let fields = extract_delta_fields(chunk);
    assert_eq!(fields.content, Some("choice0 content"));
}
