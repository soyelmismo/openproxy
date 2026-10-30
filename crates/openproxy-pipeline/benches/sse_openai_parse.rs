//! A/B for the scan-gated fast path in `parse_openai_sse_line`: measures the
//! current implementation (byte-scan gate) against the pre-change behaviour
//! (unconditional `serde_json` probe parse) on a typical content-delta chunk
//! — the >90% case in a streaming response.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

const DELTA_LINE: &str = r#"data: {"id":"chatcmpl-abc","object":"chat.completion.chunk","created":1730000000,"model":"swe-2","choices":[{"index":0,"delta":{"content":"The quick brown fox jumps over the lazy dog. "},"finish_reason":null}],"system_fingerprint":"fp_x"}"#;

const USAGE_LINE: &str = r#"data: {"id":"chatcmpl-abc","object":"chat.completion.chunk","created":1730000000,"model":"swe-2","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":423,"completion_tokens":631,"total_tokens":1054,"prompt_tokens_details":{"cached_tokens":400}}}"#;

fn bench_fast_path(c: &mut Criterion) {
    let mut g = c.benchmark_group("sse_openai_parse");

    // New code path (scan-gated).
    g.bench_function("delta_scan_gated", |b| {
        b.iter(|| {
            black_box(openproxy_pipeline::sse::openai::parse_openai_sse_line(
                black_box(DELTA_LINE),
            ))
        })
    });
    g.bench_function("usage_scan_gated", |b| {
        b.iter(|| {
            black_box(openproxy_pipeline::sse::openai::parse_openai_sse_line(
                black_box(USAGE_LINE),
            ))
        })
    });

    // Old behaviour: unconditional probe parse of the payload. Reproduced
    // inline — strips `data: ` then `serde_json::from_str` into the same
    // probe-shaped struct, which is what the function did before the gate.
    #[derive(serde::Deserialize)]
    struct Probe {
        #[allow(dead_code)]
        usage: Option<serde_json::Value>,
        #[allow(dead_code)]
        choices: Option<Vec<serde_json::Value>>,
    }

    g.bench_function("delta_always_parse", |b| {
        b.iter(|| {
            let payload = DELTA_LINE.trim_start_matches("data: ");
            black_box(serde_json::from_str::<Probe>(black_box(payload)))
        })
    });
    g.bench_function("usage_always_parse", |b| {
        b.iter(|| {
            let payload = USAGE_LINE.trim_start_matches("data: ");
            black_box(serde_json::from_str::<Probe>(black_box(payload)))
        })
    });

    g.finish();
}

criterion_group!(benches, bench_fast_path);
criterion_main!(benches);
