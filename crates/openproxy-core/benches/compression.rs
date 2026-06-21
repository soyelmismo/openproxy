//! Benchmark for the compression module.
//!
//! Measures `apply_compression` throughput on realistic multi-message
//! fixtures in `Lite`, `Rtk`, and `LiteRtk` modes. Run with:
//!
//!   cargo bench -p openproxy-core --bench compression
//!
//! Or for a quick numeric readout:
//!   cargo test -p openproxy-core --bench compression --release -- --nocapture

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use openproxy_core::compression::{apply_compression, CompressionMode};
use openproxy_core::translation::OpenAIRequest;
use serde_json::{json, Value};

/// Build a realistic chat request with N messages: system prompt, user
/// messages with tool outputs (git status, cargo test, etc.), and
/// assistant responses. Total ~50 KB of content.
fn build_fixture() -> Vec<openproxy_core::translation::OpenAIMessage> {
    let mut messages = Vec::new();

    // System prompt (~500 bytes, normalized).
    messages.push(openproxy_core::translation::OpenAIMessage {
        role: "system".to_string(),
        content: Some(Value::String(
            "You are a helpful coding assistant. Be concise.\n\nWhen shown command output, focus on errors and warnings.".to_string()
        )),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: serde_json::Map::new(),
    });

    // User message: "run git status"
    messages.push(openproxy_core::translation::OpenAIMessage {
        role: "user".to_string(),
        content: Some(Value::String("Run `git status` and tell me what changed.".to_string())),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: serde_json::Map::new(),
    });

    // Assistant tool_call
    messages.push(openproxy_core::translation::OpenAIMessage {
        role: "assistant".to_string(),
        content: Some(Value::Null),
        name: None,
        tool_call_id: None,
        tool_calls: Some(vec![json!({
            "id": "call_1",
            "type": "function",
            "function": {"name": "run_command", "arguments": "{\"cmd\":\"git status\"}"}
        })]),
        extra: serde_json::Map::new(),
    });

    // Tool result: git status output (~2 KB, with ANSI colors)
    let mut git_status = String::with_capacity(2048);
    git_status.push_str("\x1b[33m## main\x1b[0m\n");
    git_status.push_str("\x1b[31mM  src/main.rs\x1b[0m\n");
    git_status.push_str("\x1b[32mA  src/new_file.rs\x1b[0m\n");
    git_status.push_str("\x1b[31m D src/removed.rs\x1b[0m\n");
    git_status.push_str("?? src/untracked.rs\n");
    // Pad with more lines to make it realistic (~2 KB).
    for i in 0..40 {
        git_status.push_str(&format!("\x1b[33mM  src/file_{}.rs\x1b[0m\n", i));
    }
    messages.push(openproxy_core::translation::OpenAIMessage {
        role: "tool".to_string(),
        content: Some(Value::String(git_status)),
        name: None,
        tool_call_id: Some("call_1".to_string()),
        tool_calls: None,
        extra: serde_json::Map::new(),
    });

    // User: "run cargo test"
    messages.push(openproxy_core::translation::OpenAIMessage {
        role: "user".to_string(),
        content: Some(Value::String("Now run `cargo test`.".to_string())),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: serde_json::Map::new(),
    });

    // Assistant tool_call
    messages.push(openproxy_core::translation::OpenAIMessage {
        role: "assistant".to_string(),
        content: Some(Value::Null),
        name: None,
        tool_call_id: None,
        tool_calls: Some(vec![json!({
            "id": "call_2",
            "type": "function",
            "function": {"name": "run_command", "arguments": "{\"cmd\":\"cargo test\"}"}
        })]),
        extra: serde_json::Map::new(),
    });

    // Tool result: cargo test output (~5 KB, with ANSI and trailing whitespace)
    let mut cargo_test = String::with_capacity(5120);
    cargo_test.push_str("\x1b[1m\x1b[32m    Finished test [unoptimized + debuginfo] target(s) in 0.52s\x1b[0m\n");
    cargo_test.push_str("     Running unittests src/lib.rs\n");
    cargo_test.push_str("\n");
    cargo_test.push_str("running 25 tests\n");
    for i in 0..25 {
        cargo_test.push_str(&format!("test tests::test_{} ... ok\n", i));
    }
    cargo_test.push_str("\n");
    cargo_test.push_str("test result: ok. 25 passed; 0 failed; 0 ignored; 0 measured; 100% filtered out\n");
    // Add some trailing whitespace (what normalize_message_whitespace should trim).
    for i in 0..30 {
        cargo_test.push_str(&format!("warning: unused variable `x` in src/file_{}.rs:42:13   \n", i));
    }
    // Pad with error-stacktrace-like content.
    cargo_test.push_str("\nthread 'tests::test_panic' panicked at 'index out of bounds':\n");
    cargo_test.push_str("   0: std::panicking::begin_panic\n");
    cargo_test.push_str("   1: src/lib.rs:123:5\n");
    cargo_test.push_str("   2: src/lib.rs:456:7\n");
    cargo_test.push_str("note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n");
    messages.push(openproxy_core::translation::OpenAIMessage {
        role: "tool".to_string(),
        content: Some(Value::String(cargo_test)),
        name: None,
        tool_call_id: Some("call_2".to_string()),
        tool_calls: None,
        extra: serde_json::Map::new(),
    });

    // Final user message
    messages.push(openproxy_core::translation::OpenAIMessage {
        role: "user".to_string(),
        content: Some(Value::String("What should I do next?".to_string())),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: serde_json::Map::new(),
    });

    messages
}

fn bench_compression(c: &mut Criterion) {
    let mut group = c.benchmark_group("compression");

    // Measure each mode. We clone the fixture per iteration because
    // apply_compression mutates the messages in place.
    group.bench_function("lite", |b| {
        b.iter_with_setup(
            || build_fixture(),
            |mut msgs| {
                let stats = apply_compression(&mut msgs, CompressionMode::Lite);
                black_box((stats, msgs));
            },
        );
    });

    group.bench_function("rtk", |b| {
        b.iter_with_setup(
            || build_fixture(),
            |mut msgs| {
                let stats = apply_compression(&mut msgs, CompressionMode::Rtk);
                black_box((stats, msgs));
            },
        );
    });

    group.bench_function("lite_rtk", |b| {
        b.iter_with_setup(
            || build_fixture(),
            |mut msgs| {
                let stats = apply_compression(&mut msgs, CompressionMode::LiteRtk);
                black_box((stats, msgs));
            },
        );
    });

    group.finish();
}

criterion_group!(benches, bench_compression);
criterion_main!(benches);

// Suppress unused-import warning for OpenAIRequest (kept for documentation).
#[allow(unused_imports)]
use _OpenAIRequest as _;
type _OpenAIRequest = OpenAIRequest;
