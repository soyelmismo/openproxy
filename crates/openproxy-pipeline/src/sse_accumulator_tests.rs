use super::*;

#[test]
fn empty_accumulator_produces_minimal_response() {
    let acc = ResponseAccumulator::new();
    let v = acc.finish("chatcmpl-test", 1234, "test-model");
    assert_eq!(v["id"], "chatcmpl-test");
    assert_eq!(v["model"], "test-model");
    assert_eq!(v["choices"][0]["message"]["role"], "assistant");
    assert_eq!(v["choices"][0]["message"]["content"], Value::Null);
    assert_eq!(v["choices"][0]["finish_reason"], Value::Null);
    assert!(v.get("usage").is_none());
}

#[test]
fn openai_raw_payloads_concatenate_content() {
    let mut acc = ResponseAccumulator::new();
    acc.append_openai_raw(r#"{"choices":[{"delta":{"content":"hi"}}]}"#);
    acc.append_openai_raw(r#"{"choices":[{"delta":{"content":" there"}}]}"#);
    assert_eq!(
        acc.finish("id", 0, "m")["choices"][0]["message"]["content"],
        "hi there"
    );
}

#[test]
fn openai_raw_payloads_multibyte_utf8_boundaries() {
    let mut acc = ResponseAccumulator::new();
    acc.append_openai_raw(r#"{"choices":[{"delta":{"content":"при"}}]}"#);
    acc.append_openai_raw(r#"{"choices":[{"delta":{"content":"вет"}}]}"#);
    assert_eq!(
        acc.finish("id", 0, "m")["choices"][0]["message"]["content"],
        "привет"
    );
}

#[test]
fn openai_raw_payloads_mid_stream_malformed_json() {
    let mut acc = ResponseAccumulator::new();
    acc.append_openai_raw(r#"{"choices":[{"delta":{"content":"good"}}]}"#);
    acc.append_openai_raw(r#"{"choices":[{"delta":{"content":" malformed"#);
    acc.append_openai_raw(r#"{"choices":[{"delta":{"content":" bye"}}]}"#);
    assert_eq!(
        acc.finish("id", 0, "m")["choices"][0]["message"]["content"],
        "good bye"
    );
}

#[test]
fn reasoning_goes_into_extra() {
    let mut acc = ResponseAccumulator::new();
    acc.append_reasoning("step 1");
    acc.append_reasoning(" + step 2");
    let v = acc.finish("id", 0, "m");
    assert_eq!(
        v["choices"][0]["message"]["reasoning_content"],
        "step 1 + step 2"
    );
}

#[test]
fn anthropic_tool_use_lifecycle() {
    let mut acc = ResponseAccumulator::new();
    acc.update_anthropic_tool_use(AnthropicToolEvent::Open(Box::new(AnthropicToolOpen {
        id: "toolu_1".into(),
        name: "get_weather".into(),
    })));
    acc.update_anthropic_tool_use(AnthropicToolEvent::Delta {
        partial_json: r#"{"city":"#.into(),
    });
    acc.update_anthropic_tool_use(AnthropicToolEvent::Delta {
        partial_json: r#""Madrid"}"#.into(),
    });
    acc.update_anthropic_tool_use(AnthropicToolEvent::Close);
    let v = acc.finish("id", 0, "m");
    let tc = &v["choices"][0]["message"]["tool_calls"].as_array().unwrap()[0];
    assert_eq!(tc["id"], "toolu_1");
    assert_eq!(tc["function"]["name"], "get_weather");
    assert_eq!(tc["function"]["arguments"], r#"{"city":"Madrid"}"#);
}

#[test]
fn cap_truncates_and_sets_flag() {
    let mut acc = ResponseAccumulator::new();
    let big = "x".repeat(MAX_ACCUMULATED_BYTES);
    acc.append_openai_raw(&format!(
        r#"{{"choices":[{{"delta":{{"content":"{big}"}}}}]}}"#
    ));
    assert!(!acc.is_truncated());
    acc.append_openai_raw(r#"{"choices":[{"delta":{"content":"more"}}]}"#);
    assert!(acc.is_truncated());
    assert_eq!(
        acc.finish("id", 0, "m")["choices"][0]["message"]["truncated"],
        Value::Bool(true)
    );
}

#[test]
fn usage_and_stop_reason_populated() {
    let mut acc = ResponseAccumulator::new();
    acc.set_usage(OpenAIUsage {
        prompt_tokens: 10,
        completion_tokens: 20,
        total_tokens: 30,
        prompt_tokens_details: None,
    });
    acc.set_stop_reason("stop");
    let v = acc.finish("id", 0, "m");
    assert_eq!(v["usage"]["total_tokens"], 30);
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
}

#[test]
fn partial_flag_and_content_text() {
    let mut acc = ResponseAccumulator::new();
    acc.append_openai_raw(r#"{"choices":[{"delta":{"content":"part1"}}]}"#);
    acc.append_openai_raw(r#"{"choices":[{"delta":{"content":" part2"}}]}"#);
    acc.mark_partial();
    assert!(acc.is_partial());
    assert_eq!(acc.content_text(), "part1 part2");
    let v = acc.finish("id", 0, "m");
    assert_eq!(v["choices"][0]["message"]["partial"], Value::Bool(true));
}

#[test]
fn append_reasoning_and_tool_call() {
    let mut acc = ResponseAccumulator::new();
    acc.append_reasoning("thought 1 ");
    acc.append_reasoning("thought 2");
    acc.append_openai_tool_call(Some("call_1"), "get_time", r#"{"zone":"UTC"}"#);
    let v = acc.finish("id", 0, "m");
    assert_eq!(
        v["choices"][0]["message"]["reasoning_content"],
        "thought 1 thought 2"
    );
    assert_eq!(v["choices"][0]["message"]["tool_calls"][0]["id"], "call_1");
}

#[test]
fn normalize_reasoning_field_to_reasoning_content() {
    let p = r#"{"id":"x","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"content":"","role":"assistant","reasoning":" Need"},"finish_reason":null}]}"#;
    let norm = normalize_nonstandard_reasoning_fields(p).unwrap();
    assert!(norm.contains("\"reasoning_content\"") && !norm.contains("\"reasoning\":"));
    let v: Value = serde_json::from_str(&norm).unwrap();
    assert_eq!(v["choices"][0]["delta"]["reasoning_content"], " Need");
}

#[test]
fn normalize_reasoning_details_array() {
    let p = r#"{"choices":[{"delta":{"reasoning_details":[{"text":"Need"},{"text":" to"}]}}]}"#;
    let norm = normalize_nonstandard_reasoning_fields(p).unwrap();
    let v: Value = serde_json::from_str(&norm).unwrap();
    assert_eq!(v["choices"][0]["delta"]["reasoning_content"], "Need to");
    assert!(v["choices"][0]["delta"].get("reasoning_details").is_none());
}

#[test]
fn normalize_standard_and_none_reasoning() {
    assert!(
        normalize_nonstandard_reasoning_fields(
            r#"{"choices":[{"delta":{"reasoning_content":"hi"}}]}"#
        )
        .is_none()
    );
    assert!(
        normalize_nonstandard_reasoning_fields(r#"{"choices":[{"delta":{"content":"hi"}}]}"#)
            .is_none()
    );
    assert_eq!(
        extract_reasoning_content(r#"{"choices":[{"delta":{"reasoning_content":" step"}}]}"#),
        Some(" step")
    );
    assert!(extract_reasoning_content(r#"{"choices":[{"delta":{"content":"hi"}}]}"#).is_none());
}

#[test]
fn normalize_both_reasoning_and_details() {
    let p =
        r#"{"choices":[{"delta":{"reasoning":"think","reasoning_details":[{"text":" more"}]}}]}"#;
    let v: Value =
        serde_json::from_str(&normalize_nonstandard_reasoning_fields(p).unwrap()).unwrap();
    assert_eq!(v["choices"][0]["delta"]["reasoning_content"], "think");
    assert!(v["choices"][0]["delta"].get("reasoning_details").is_none());
}

#[test]
fn raw_response_body_and_errors() {
    let mut acc = ResponseAccumulator::new();
    assert!(acc.is_completely_empty());
    acc.append_raw_line("data: raw line");
    assert!(!acc.is_completely_empty() && acc.raw_response_body().contains("raw line"));

    let err_p = r#"data: {"choices":[],"error":{"code":502,"message":"Upstream error: ResourceExhausted"}}"#;
    let mut err_acc = ResponseAccumulator::new();
    err_acc.append_raw_line(err_p);
    let (code, msg) = err_acc.extract_upstream_error_from_raw().unwrap();
    assert_eq!(code, 502);
    assert!(msg.contains("ResourceExhausted"));

    let mut cf_acc = ResponseAccumulator::new();
    cf_acc
        .append_raw_line(r#"data: {"error": {"message": "I'm sorry", "code": "content_filter"}}"#);
    assert_eq!(cf_acc.extract_upstream_error_from_raw().unwrap().0, 400);

    let mut def_acc = ResponseAccumulator::new();
    def_acc.append_raw_line(r#"data: {"error":{"message":"unknown"}}"#);
    assert_eq!(def_acc.extract_upstream_error_from_raw().unwrap().0, 502);
}

#[test]
fn test_escape_decoding_accumulates_unescaped_newlines() {
    let mut acc = ResponseAccumulator::new();
    acc.append_openai_raw(r#"{"choices":[{"delta":{"content":"Line 1\nLine 2\nLine 3"}}]}"#);
    let v = acc.finish("chatcmpl-test", 1234, "test-model");
    let content = v["choices"][0]["message"]["content"].as_str().unwrap();
    assert_eq!(content, "Line 1\nLine 2\nLine 3");
    assert_eq!(content.matches('\n').count(), 2);
    let serialized = serde_json::to_string(&v).unwrap();
    assert!(serialized.contains(r#""content":"Line 1\nLine 2\nLine 3""#));
    assert!(!serialized.contains(r"\\n"));
}

#[test]
fn test_escape_decoding_all_standard_sequences() {
    let mut acc = ResponseAccumulator::new();
    acc.append_openai_raw(r#"{"choices":[{"delta":{"content":"quote:\" backslash:\\ tab:\t slash:\/ unicode:\u0026"}}]}"#);
    let v = acc.finish("chatcmpl-test", 1234, "test-model");
    let content = v["choices"][0]["message"]["content"].as_str().unwrap();
    assert_eq!(content, "quote:\" backslash:\\ tab:\t slash:/ unicode:&");
}

#[test]
fn test_tool_calls_nested_content_does_not_pollute_message_content() {
    let mut acc = ResponseAccumulator::new();
    let chunk = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_123","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"test.txt\",\"content\":\"file content here\"}"}}]}}]}"#;
    acc.append_openai_raw(chunk);
    let v = acc.finish("chatcmpl-test", 1234, "test-model");
    assert_eq!(v["choices"][0]["message"]["content"], Value::Null);
    let tc = &v["choices"][0]["message"]["tool_calls"];
    assert!(tc.is_array());
    assert_eq!(tc.as_array().unwrap().len(), 1);
    assert_eq!(tc[0]["function"]["name"], "write_file");
    assert_eq!(tc[0]["function"]["arguments"], "{\"path\":\"test.txt\",\"content\":\"file content here\"}");
}

#[test]
fn test_adversarial_mixed_escapes_and_surrogate_pairs() {
    let mut acc = ResponseAccumulator::new();
    // \n, \", \\, \t, \r, \u0041 (A), and surrogate pair \uD83D\uDE00 (😀)
    let payload = r#"{"choices":[{"delta":{"content":"Line 1\nTab:\tCR:\rQuote:\"Backslash:\\Slash:\/Hex:\u0041Emoji:\uD83D\uDE00"}}]}"#;
    acc.append_openai_raw(payload);
    let v = acc.finish("chatcmpl-test", 1234, "test-model");
    let content = v["choices"][0]["message"]["content"].as_str().unwrap();
    assert_eq!(content, "Line 1\nTab:\tCR:\rQuote:\"Backslash:\\Slash:/Hex:AEmoji:😀");

    // Single-layer escaping in serialized output
    let serialized = serde_json::to_string(&v).unwrap();
    // Must NOT contain double-escaped \\n or \\" or \\\\
    assert!(!serialized.contains(r"\\n"));
    assert!(!serialized.contains(r"\\t"));
    assert!(!serialized.contains(r"\\r"));
    // Must contain single-escaped newline in JSON string representation: "Line 1\nTab:\tCR:\rQuote:\"Backslash:\\Slash:/Hex:AEmoji:😀"
    let re_parsed: Value = serde_json::from_str(&serialized).unwrap();
    assert_eq!(
        re_parsed["choices"][0]["message"]["content"],
        "Line 1\nTab:\tCR:\rQuote:\"Backslash:\\Slash:/Hex:AEmoji:😀"
    );
}

#[test]
fn test_adversarial_deeply_nested_tool_call_arguments() {
    let mut acc = ResponseAccumulator::new();
    let chunk = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_999","type":"function","function":{"name":"write_file","arguments":"{\"options\":{\"content\":\"nested sensitive payload\",\"deep\":{\"content\":\"inner\"}}}"}}]}}]}"#;
    acc.append_openai_raw(chunk);
    let v = acc.finish("chatcmpl-test", 1234, "test-model");
    assert_eq!(v["choices"][0]["message"]["content"], Value::Null);
    let tc = &v["choices"][0]["message"]["tool_calls"];
    assert!(tc.is_array());
    assert_eq!(tc.as_array().unwrap()[0]["function"]["name"], "write_file");
}

#[test]
fn test_adversarial_invalid_unicode_escape_multibyte_no_panic() {
    let test_cases = [
        r"\u123ñ",
        r"test\u123ñend",
        r"\uD800\u123ñ",
        r"\uD800\uDC0ñ",
        r"\uññññ",
        r"prefix\u00e9suffix",
        r"\u",
        r"\u1",
        r"\u12",
        r"\u123",
        r"\uD800",
        r"\uD800\u",
        r"\uD800\uDC",
    ];

    for case in test_cases {
        let mut out = Vec::new();
        decode_json_escape_into(case, &mut out);
        // Verify output is valid UTF-8 and does not panic
        let decoded = String::from_utf8(out).expect("decoded bytes must be valid utf8");
        // Non-panic and progress guaranteed
        assert!(!decoded.is_empty() || case.is_empty());
    }

    // Specific verification of r"\u123ñ"
    let mut out_malformed = Vec::new();
    decode_json_escape_into(r"\u123ñ", &mut out_malformed);
    let s = String::from_utf8(out_malformed).expect("valid utf8");
    assert_eq!(s, r"\u123ñ");
}

#[test]
fn test_adversarial_empirical_challenger_stress_utf8() {
    // 1. Mandatory test strings from challenger mission
    let mandatory_cases = [
        r"\u123ñ",
        r"\uD800\u123ñ",
        r"\uññññ",
        r"\u",
    ];

    for case in &mandatory_cases {
        let mut out = Vec::new();
        decode_json_escape_into(case, &mut out);
        let s = String::from_utf8(out).expect("output must be valid utf-8");
        assert!(!s.is_empty() || case.is_empty());
    }

    // 2. Comprehensive edge case suite (surrogate pairs, 4-byte emojis, CJK, partial escapes)
    let edge_cases = [
        r"\uD83D\uDE00", // Valid emoji 😀
        r"\uD83D\uDE02", // Valid emoji 😂
        r"\uD83D\u123ñ", // High surrogate + malformed low escape with multibyte
        r"\uD83D\uññññ", // High surrogate + 4 multibyte chars
        r"\uD83D\u",     // High surrogate + truncated \u
        r"\uD83D\u1",    // High surrogate + 1 hex digit
        r"\uD83D\u12",   // High surrogate + 2 hex digits
        r"\uD83D\u123",  // High surrogate + 3 hex digits
        r"\u123🦀",      // 4-byte UTF-8 boundary split
        r"\u12🦀",       // 4-byte UTF-8 character inside escape
        r"\u🦀",         // 4-byte UTF-8 character immediately after \u
        r"\u4e16\u754c", // CJK characters 世界
        r"\u0000",       // Null byte
        r"\",            // Lone trailing backslash
        r"\\",           // Escaped backslash
        r"\\\",          // Triple backslash
        r"\\\\",         // Quadruple backslash
        r"\uD800\uD800", // High surrogate followed by high surrogate
        r"\uDC00\uDC00", // Low surrogate followed by low surrogate
        r"\uDC00\uD800", // Inverted surrogates
        r"\uFFFF",       // Max BMP
        r#"\b\f\n\r\t\/\"\\"#, // Standard JSON escapes
        r"\a\e\v\z",     // Non-standard escapes (treated as literal backslashes)
        r"prefix\u0041middle\u123ñsuffix\uD83D\uDE00end🦀",
    ];

    for case in &edge_cases {
        let res = std::panic::catch_unwind(|| {
            let mut out = Vec::new();
            decode_json_escape_into(case, &mut out);
            String::from_utf8(out).expect("must be valid utf8")
        });
        assert!(res.is_ok(), "Panic on adversarial case: {case}");
    }

    // Specific check for surrogate pair decoding
    let mut emoji_out = Vec::new();
    decode_json_escape_into(r"\uD83D\uDE00", &mut emoji_out);
    assert_eq!(String::from_utf8(emoji_out).unwrap(), "😀");

    // 3. Fuzzing / Combinatorial stress test (2,000 combinations)
    let fragments = [
        r"\", r"\u", r"\u1", r"\u12", r"\u123", r"\u1234",
        r"\uD800", r"\uD83D", r"\uDC00", r"\uDE00",
        "ñ", "€", "中", "🦀", "🌟", "A", "0", "\"", "\n", "\r",
        r"\uñ", r"\u1ñ", r"\u12ñ", r"\u123ñ",
    ];

    let mut state: u64 = 0xdeadbeef12345678;
    for _ in 0..2_000 {
        let mut test_str = String::new();
        for _ in 0..8 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let idx = ((state >> 32) as usize) % fragments.len();
            test_str.push_str(fragments[idx]);
        }

        let res = std::panic::catch_unwind(|| {
            let mut out = Vec::new();
            decode_json_escape_into(&test_str, &mut out);
            String::from_utf8(out).expect("decoded bytes must always be valid UTF-8")
        });
        assert!(res.is_ok(), "decode_json_escape_into panicked on randomized string: {test_str:?}");
    }

    // 4. End-to-end integration with ResponseAccumulator
    let mut acc = ResponseAccumulator::new();
    let malformed_chunk = r#"{"id":"test","choices":[{"index":0,"delta":{"content":"Hello \u123ñ \uD800\u123ñ \uññññ \u \uD83D\uDE00 🦀 world"}}]}"#;
    acc.append_openai_raw(malformed_chunk);
    let finished = acc.finish("test-id", 100, "test-model");
    let content = finished["choices"][0]["message"]["content"].as_str().expect("string content");
    assert!(content.contains("Hello"));
    assert!(content.contains("😀"));
    assert!(content.contains("🦀"));
    assert!(content.contains("world"));
}


