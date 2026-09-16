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
