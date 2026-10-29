//! Reproduction with the EXACT content captured from the failing request
//! (usage id 924727, trace 21b20348). The payload is loaded from a fixture
//! written straight out of the captured `response_body_json`, so the bytes
//! cannot drift from the real failure.

use crate::inline_tools::extract_inline_tools_from_response_value;

/// Verbatim `response_body_json.choices[0].message.content` from the failure.
fn real_dsml_content() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/real_dsml_content.txt");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()))
}

fn real_response_body(content: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-7543a0d7-bd85-4f24-9fa3-b9c8c7705477",
        "object": "chat.completion",
        "created": 1793251173u64,
        "model": "deepseek-v4.1-flash:free",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": content,
                "reasoning_content": "The user asked for a short video about..."
            },
            "finish_reason": null
        }]
    })
}

#[test]
fn real_failure_content_parses_into_terminal_call() {
    let content = real_dsml_content();
    assert_eq!(
        content.chars().count(),
        656,
        "fixture must match the capture"
    );
    assert_eq!(content.len(), 736, "fixture must match the capture bytes");

    let out = extract_inline_tools_from_response_value(real_response_body(&content));
    println!("{}", serde_json::to_string_pretty(&out).unwrap());

    let choice = &out["choices"][0];
    assert_eq!(choice["finish_reason"], "tool_calls");
    assert!(choice["message"]["content"].is_null());

    let call = &choice["message"]["tool_calls"][0];
    assert_eq!(call["function"]["name"], "terminal");
    assert_eq!(call["type"], "function");

    let args: serde_json::Value =
        serde_json::from_str(call["function"]["arguments"].as_str().unwrap())
            .expect("arguments must be valid JSON");
    assert!(
        args["command"]
            .as_str()
            .unwrap()
            .starts_with("cd ~/sandbox/explainroo && export EXPLAINROO_CACHE=$PWD/.cache"),
        "command must survive verbatim: {args}"
    );
    assert_eq!(
        args["intent"],
        "Verificando el MP4 final y listando salidas"
    );
    assert_eq!(
        args["timeout_secs"], 600,
        "numeric string must become a number"
    );
}

#[test]
fn real_failure_content_streamed_in_pieces_parses_identically() {
    use crate::inline_tools::InlineToolStreamExtractor;
    use crate::streaming::{StreamAction, StreamingChunkStage};

    let content = real_dsml_content();
    let mut ex = InlineToolStreamExtractor::new();
    let mut emitted: Vec<serde_json::Value> = Vec::new();

    for ch in content.chars() {
        let payload = serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": { "content": ch.to_string() },
                "finish_reason": null
            }]
        })
        .to_string();
        if let StreamAction::Mutate(m) = ex.process_chunk(&payload) {
            let v: serde_json::Value = serde_json::from_str(&m).unwrap();
            if let Some(tcs) = v
                .pointer("/choices/0/delta/tool_calls")
                .and_then(|t| t.as_array())
            {
                emitted.extend(tcs.iter().cloned());
            }
        }
    }
    if let Some(tail) = ex.finalize() {
        let v: serde_json::Value = serde_json::from_str(&tail).unwrap();
        if let Some(tcs) = v
            .pointer("/choices/0/delta/tool_calls")
            .and_then(|t| t.as_array())
        {
            emitted.extend(tcs.iter().cloned());
        }
    }

    println!(
        "emitted tool calls: {}",
        serde_json::to_string_pretty(&emitted).unwrap()
    );
    assert_eq!(emitted.len(), 1, "exactly one call expected");
    assert_eq!(emitted[0]["function"]["name"], "terminal");

    let args: serde_json::Value =
        serde_json::from_str(emitted[0]["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(
        args["intent"],
        "Verificando el MP4 final y listando salidas"
    );
    assert_eq!(args["timeout_secs"], 600);
}
