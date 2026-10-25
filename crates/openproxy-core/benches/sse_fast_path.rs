#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Benchmark for the SSE chunk-forwarding hot path.
//!
//! Inner loop of `Pipeline::dispatch_upstream_streaming` on the OpenAI fast path
//! (content-only chunks, no `usage` / `finish_reason`), measured per chunk for two
//! strategies:
//!
//!   1. OLD: allocate a fresh `BytesMut` per chunk and copy `data: ` + payload + `\n\n`
//!   2. NEW: reuse `line_bytes` and append `\n\n` in place
//!
//! Run with `cargo bench -p openproxy-core --bench sse_fast_path`.

use bytes::{Bytes, BytesMut};
use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

/// OpenAI chunks: small content delta, no usage, no finish_reason (the >99% fast-path shape).
const SAMPLE_CHUNKS: &[&str] = &[
    r#"data: {"id":"chatcmpl-X","object":"chat.completion.chunk","created":1700000000,"model":"gpt-4o","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#,
    r#"data: {"id":"chatcmpl-X","object":"chat.completion.chunk","created":1700000000,"model":"gpt-4o","choices":[{"index":0,"delta":{"content":", "},"finish_reason":null}]}"#,
    r#"data: {"id":"chatcmpl-X","object":"chat.completion.chunk","created":1700000000,"model":"gpt-4o","choices":[{"index":0,"delta":{"content":"world"},"finish_reason":null}]}"#,
    r#"data: {"id":"chatcmpl-X","object":"chat.completion.chunk","created":1700000000,"model":"gpt-4o","choices":[{"index":0,"delta":{"content":"!"},"finish_reason":null}]}"#,
    r#"data: {"id":"chatcmpl-X","object":"chat.completion.chunk","created":1700000000,"model":"gpt-4o","choices":[{"index":0,"delta":{"content":" How"},"finish_reason":null}]}"#,
    r#"data: {"id":"chatcmpl-X","object":"chat.completion.chunk","created":1700000000,"model":"gpt-4o","choices":[{"index":0,"delta":{"content":" are"},"finish_reason":null}]}"#,
    r#"data: {"id":"chatcmpl-X","object":"chat.completion.chunk","created":1700000000,"model":"gpt-4o","choices":[{"index":0,"delta":{"content":" you"},"finish_reason":null}]}"#,
    r#"data: {"id":"chatcmpl-X","object":"chat.completion.chunk","created":1700000000,"model":"gpt-4o","choices":[{"index":0,"delta":{"content":" doing"},"finish_reason":null}]}"#,
    r#"data: {"id":"chatcmpl-X","object":"chat.completion.chunk","created":1700000000,"model":"gpt-4o","choices":[{"index":0,"delta":{"content":" today"},"finish_reason":null}]}"#,
    r#"data: {"id":"chatcmpl-X","object":"chat.completion.chunk","created":1700000000,"model":"gpt-4o","choices":[{"index":0,"delta":{"content":"?"},"finish_reason":null}]}"#,
];

/// OLD path: fresh BytesMut holding `data: ` + payload + `\n\n`.
fn old_reframe(line_bytes: &BytesMut) -> Bytes {
    // mirrors the real str conversion + strip_prefix + trim_start
    let line = std::str::from_utf8(line_bytes).unwrap();
    let json_payload = line.strip_prefix("data:").unwrap().trim_start();
    let mut sse_frame = BytesMut::with_capacity(json_payload.len() + 16);
    sse_frame.extend_from_slice(b"data: ");
    sse_frame.extend_from_slice(json_payload.as_bytes());
    sse_frame.extend_from_slice(b"\n\n");
    sse_frame.freeze()
}

/// NEW path: reuse `line_bytes`, append `\n\n`, freeze.
fn new_reframe(mut line_bytes: BytesMut) -> Bytes {
    line_bytes.extend_from_slice(b"\n\n");
    line_bytes.freeze()
}

/// `split_to(pos)` returns a BytesMut holding the line bytes with the parent's spare
/// capacity; without that spare capacity `extend_from_slice(b"\n\n")` in the NEW path
/// would realloc.
fn make_line_with_spare_capacity(chunk: &str) -> BytesMut {
    let mut buf = BytesMut::with_capacity(8192);
    buf.extend_from_slice(chunk.as_bytes());
    // the parent capacity gives 8192 - chunk.len() bytes of spare capacity
    buf
}

/// Build a `data: <payload>` BytesMut per sample chunk, without the trailing newline.
fn bench_old(c: &mut Criterion) {
    let mut group = c.benchmark_group("openai_fast_path_reframe");
    group.throughput(criterion::Throughput::Elements(SAMPLE_CHUNKS.len() as u64));
    group.bench_function("old_alloc_per_chunk", |b| {
        b.iter(|| {
            let mut total: u64 = 0;
            for chunk in SAMPLE_CHUNKS {
                // `split_to(pos)` yields line bytes plus spare capacity
                let line_bytes = make_line_with_spare_capacity(chunk);
                let frame = old_reframe(&line_bytes);
                total += frame.len() as u64;
            }
            black_box(total);
        });
    });
    group.bench_function("new_reuse_in_place", |b| {
        b.iter(|| {
            let mut total: u64 = 0;
            for chunk in SAMPLE_CHUNKS {
                let line_bytes = make_line_with_spare_capacity(chunk);
                let frame = new_reframe(line_bytes);
                total += frame.len() as u64;
            }
            black_box(total);
        });
    });
    group.finish();
}

/// Gemini probe-struct parse against the old Value-based parse.
fn bench_gemini_parse(c: &mut Criterion) {
    use openproxy_pipeline::sse::parse_gemini_sse_line_test as parse_gemini_sse_line;

    // `]}` closes the parts array and content object before the candidates separator comma
    const GEMINI_CHUNKS: &[&str] = &[
        r#"data: {"candidates":[{"content":{"parts":[{"text":"Hello"}],"role":"model"}}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":1,"totalTokenCount":11}}"#,
        r#"data: {"candidates":[{"content":{"parts":[{"text":", "}],"role":"model"}}]}"#,
        r#"data: {"candidates":[{"content":{"parts":[{"text":"world"}],"role":"model"}}]}"#,
        r#"data: {"candidates":[{"content":{"parts":[{"text":"!"}],"role":"model"}}]}"#,
    ];

    let mut group = c.benchmark_group("gemini_sse_parse");
    group.throughput(criterion::Throughput::Elements(GEMINI_CHUNKS.len() as u64));
    group.bench_function("probe_struct", |b| {
        b.iter(|| {
            let mut total: u64 = 0;
            for chunk in GEMINI_CHUNKS {
                let parsed = parse_gemini_sse_line(chunk, "id", 0, "gemini-pro").unwrap();
                if let Some(c) = parsed {
                    total += c.payload.to_string().len() as u64;
                }
            }
            black_box(total);
        });
    });
    group.finish();
}

criterion_group!(benches, bench_old, bench_gemini_parse);
criterion_main!(benches);
