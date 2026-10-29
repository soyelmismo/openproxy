//! Verifies the recorded body normalization directly: what
//! `record_streaming_success` hands to the usage tracker must show structured
//! `tool_calls`, not the raw DSML text the upstream emitted.

use crate::inline_tools::extract_inline_tools_from_response_value;
use crate::think_extractor::extract_think_from_response_value;

const DSML_BODY: &str = "\n\n<｜｜DSML｜｜ calls>\n<｜｜DSML｜｜ invoke name=\"terminal\">\n<｜｜DSML｜｜ parameter name=\"command\" string=\"true\">ls -la</｜｜DSML｜｜ parameter>\n</｜｜DSML｜｜ invoke>\n</｜｜DSML｜｜ calls>";

fn normalize(val: serde_json::Value) -> serde_json::Value {
    let think_extracted = extract_think_from_response_value(val);
    extract_inline_tools_from_response_value(think_extracted)
}

#[test]
fn recorded_body_becomes_tool_calls() {
    // Shape produced by `ResponseAccumulator::finish` for the real failure:
    // DSML in `content`, `finish_reason: null`, plus `raw_response_body`.
    let acc_body = serde_json::json!({
        "id": "chatcmpl-7543a0d7-bd85-4f24-9fa3-b9c8c7705477",
        "object": "chat.completion",
        "created": 1793251173u64,
        "model": "deepseek-v4.1-flash:free",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": DSML_BODY,
                "reasoning_content": "The user asked for a short video about..."
            },
            "finish_reason": null
        }],
        "raw_response_body": "data: {\"choices\":[{\"delta\":{\"content\":\"...\"}}]}"
    });

    let out = normalize(acc_body);

    println!("{}", serde_json::to_string_pretty(&out).unwrap());

    let choice = &out["choices"][0];
    assert_eq!(choice["finish_reason"], "tool_calls");
    assert!(
        choice["message"]["content"].is_null(),
        "DSML must be stripped from recorded content"
    );
    assert_eq!(
        choice["message"]["tool_calls"][0]["function"]["name"],
        "terminal"
    );
    assert_eq!(
        choice["message"]["reasoning_content"], "The user asked for a short video about...",
        "native reasoning must survive untouched"
    );
    assert!(
        out["raw_response_body"].is_string(),
        "raw_response_body must survive the normalization"
    );
}

#[test]
fn recorded_body_keeps_plain_text_untouched() {
    let acc_body = serde_json::json!({
        "id": "x",
        "object": "chat.completion",
        "created": 1u64,
        "model": "m",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "hola, ¿qué tal?" },
            "finish_reason": "stop"
        }]
    });
    let out = normalize(acc_body.clone());
    assert_eq!(out, acc_body, "plain answers must not be rewritten");
}

#[test]
fn recorded_body_moves_think_blocks_to_reasoning() {
    let acc_body = serde_json::json!({
        "id": "x",
        "object": "chat.completion",
        "created": 1u64,
        "model": "m",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "<think>pesando</think>la respuesta"
            },
            "finish_reason": "stop"
        }]
    });
    let out = normalize(acc_body);
    let msg = &out["choices"][0]["message"];
    assert_eq!(msg["content"], "la respuesta");
    assert_eq!(msg["reasoning_content"], "pesando");
}

#[test]
fn recorded_body_with_tool_markup_and_prose_keeps_prose() {
    let content = format!("Ejecuto el comando.{DSML_BODY}");
    let acc_body = serde_json::json!({
        "id": "x",
        "object": "chat.completion",
        "created": 1u64,
        "model": "m",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": "stop"
        }]
    });
    let out = normalize(acc_body);
    let msg = &out["choices"][0]["message"];
    assert_eq!(msg["content"], "Ejecuto el comando.");
    assert_eq!(msg["tool_calls"][0]["function"]["name"], "terminal");
    assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
}

#[test]
fn recorded_body_with_existing_tool_calls_appends() {
    let acc_body = serde_json::json!({
        "id": "x",
        "object": "chat.completion",
        "created": 1u64,
        "model": "m",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": DSML_BODY,
                "tool_calls": [{
                    "id": "call_native",
                    "type": "function",
                    "function": { "name": "read_file", "arguments": "{}" }
                }]
            },
            "finish_reason": "tool_calls"
        }]
    });
    let out = normalize(acc_body);
    let calls = out["choices"][0]["message"]["tool_calls"]
        .as_array()
        .expect("tool_calls array");
    assert_eq!(calls.len(), 2, "native call must not be dropped: {calls:?}");
    assert_eq!(calls[0]["function"]["name"], "read_file");
    assert_eq!(calls[1]["function"]["name"], "terminal");
}
