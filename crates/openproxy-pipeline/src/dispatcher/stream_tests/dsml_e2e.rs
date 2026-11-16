//! End-to-end reproduction of the DeepSeek DSML case:
//! upstream emits a `<｜｜DSML｜｜ calls>` block inside `delta.content`, the
//! client declared no `tools`, and we assert what the client receives and
//! what the accumulator records.

use super::dsml_char::{DSML_BODY, dsml_chunk_line as chunk_line, run_dsml_stream_case};
use std::fmt::Write as _;

async fn run_case(upstream_frames: String) -> (Vec<String>, String) {
    let (received, conn_arc) =
        run_dsml_stream_case("dsml-e2e", "t-dsml-e2e", true, 64, upstream_frames).await;
    let conn = conn_arc.lock();
    let journal_depth: i64 = conn
        .query_row("SELECT count(*) FROM usage_journal", [], |r| r.get(0))
        .unwrap_or(-1);
    println!("journal depth = {journal_depth}");
    let recorded: String = conn
        .query_row(
            "SELECT COALESCE(payload,'<null>') FROM usage_journal ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap_or_else(|e| format!("<query error: {e}>"));
    (received, recorded)
}

fn extract_recorded_response_body(journal_payload: &str) -> Option<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(journal_payload).ok()?;
    let usage = v
        .get("RecordAttempt")
        .and_then(|r| r.get("usage_input"))
        .or_else(|| v.get("RecordUsage"))?;
    usage.get("response_body_json").cloned()
}

#[tokio::test(flavor = "multi_thread")]
async fn dsml_single_chunk_emits_tool_calls() {
    let mut frames = String::new();
    frames.push_str(&chunk_line(
        serde_json::json!({ "role": "assistant", "content": "", "reasoning_content": "thinking..." }),
        serde_json::Value::Null,
    ));
    frames.push_str(&chunk_line(
        serde_json::json!({ "content": DSML_BODY }),
        serde_json::Value::Null,
    ));
    let _ = write!(
        frames,
        "data: {}\n\n",
        serde_json::json!({
            "id": "chatcmpl-dsml",
            "object": "chat.completion.chunk",
            "created": 1793251173u64,
            "model": "deepseek-v4.1-flash:free",
            "choices": [{ "index": 0, "delta": {}, "finish_reason": serde_json::Value::Null }],
            "usage": { "prompt_tokens": 55254, "completion_tokens": 336, "total_tokens": 55590 }
        })
    );
    frames.push_str("data: [DONE]\n\n");

    let (received, recorded) = run_case(frames).await;

    println!("\n=== CLIENT CHUNKS ({}) ===", received.len());
    for (i, c) in received.iter().enumerate() {
        println!("[{i}] {}", c.trim_end());
    }
    println!("\n=== RECORDED response_body_json ===\n{recorded}\n");

    if let Some(recorded_body) = extract_recorded_response_body(&recorded) {
        let rec_choice = &recorded_body["choices"][0];
        assert_eq!(
            rec_choice["finish_reason"], "tool_calls",
            "recorded body must carry tool_calls finish_reason:\n{recorded_body}"
        );
        assert_eq!(
            rec_choice["message"]["tool_calls"][0]["function"]["name"], "terminal",
            "recorded body must carry the parsed tool call:\n{recorded_body}"
        );
        assert!(
            rec_choice["message"]["content"].is_null(),
            "recorded content must be emptied of the DSML block:\n{recorded_body}"
        );
    }

    let all: String = received.concat();
    assert!(
        all.contains("tool_calls"),
        "client must receive tool_calls, got:\n{all}"
    );
    assert!(
        !all.contains("DSML"),
        "raw DSML must never reach the client, got:\n{all}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dsml_split_chunks_emit_tool_calls() {
    let mut frames = String::new();
    frames.push_str(&chunk_line(
        serde_json::json!({ "role": "assistant", "content": "", "reasoning_content": "thinking..." }),
        serde_json::Value::Null,
    ));
    let mut chars = DSML_BODY.chars().peekable();
    let mut buf = String::new();
    while let Some(c) = chars.next() {
        buf.push(c);
        if buf.chars().count() >= 12 || chars.peek().is_none() {
            frames.push_str(&chunk_line(
                serde_json::json!({ "content": std::mem::take(&mut buf) }),
                serde_json::Value::Null,
            ));
        }
    }
    let _ = write!(
        frames,
        "data: {}\n\n",
        serde_json::json!({
            "id": "chatcmpl-dsml",
            "object": "chat.completion.chunk",
            "created": 1793251173u64,
            "model": "deepseek-v4.1-flash:free",
            "choices": [{ "index": 0, "delta": {}, "finish_reason": serde_json::Value::Null }],
            "usage": { "prompt_tokens": 55254, "completion_tokens": 336, "total_tokens": 55590 }
        })
    );
    frames.push_str("data: [DONE]\n\n");

    let (received, recorded) = run_case(frames).await;

    println!("\n=== SPLIT: CLIENT CHUNKS ({}) ===", received.len());
    for (i, c) in received.iter().enumerate() {
        println!("[{i}] {}", c.trim_end());
    }
    println!("\n=== SPLIT: RECORDED ===\n{recorded}\n");

    let all: String = received.concat();
    assert!(
        all.contains("tool_calls"),
        "client must receive tool_calls, got:\n{all}"
    );
    assert!(
        !all.contains("DSML"),
        "raw DSML must never reach the client, got:\n{all}"
    );
}
