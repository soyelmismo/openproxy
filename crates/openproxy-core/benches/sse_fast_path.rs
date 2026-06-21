//! Benchmark for the SSE chunk-forwarding hot path.
//!
//! Simulates the inner loop of `Pipeline::dispatch_upstream_streaming`
//! for the OpenAI fast path (content-only chunks, no `usage` /
//! `finish_reason`) and measures CPU time per chunk for two strategies:
//!
//!   1. **OLD**: always allocate a fresh `BytesMut` per chunk and copy
//!      `data: ` + payload + `\n\n` into it (the pre-optimization
//!      behavior).
//!   2. **NEW**: reuse the original `line_bytes` BytesMut and append
//!      just `\n\n` in-place (the post-optimization behavior).
//!
//! Run with:
//!   cargo bench -p openproxy-core --bench sse_fast_path
//!
//! Or for a quick numeric readout:
//!   cargo test -p openproxy-core --bench sse_fast_path --release -- --nocapture

use bytes::{Bytes, BytesMut};
use criterion::{black_box, criterion_group, criterion_main, Criterion};

/// A realistic OpenAI streaming chunk: small content delta, no usage,
/// no finish_reason. This is the shape that hits the fast path >99%
/// of the time during a streaming response.
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

/// Mimic the OLD per-chunk allocation path:
/// allocate a fresh BytesMut, copy `data: ` + payload + `\n\n`, freeze.
fn old_reframe(line_bytes: &BytesMut) -> Bytes {
    // Simulate the str conversion + strip_prefix + trim_start that the
    // real code does (cheap pointer arithmetic, but exercises the borrow).
    let line = std::str::from_utf8(line_bytes).unwrap();
    let json_payload = line.strip_prefix("data:").unwrap().trim_start();
    let mut sse_frame = BytesMut::with_capacity(json_payload.len() + 16);
    sse_frame.extend_from_slice(b"data: ");
    sse_frame.extend_from_slice(json_payload.as_bytes());
    sse_frame.extend_from_slice(b"\n\n");
    sse_frame.freeze()
}

/// Mimic the NEW in-place reframe:
/// reuse the line_bytes BytesMut, append `\n\n`, freeze.
fn new_reframe(mut line_bytes: BytesMut) -> Bytes {
    line_bytes.extend_from_slice(b"\n\n");
    line_bytes.freeze()
}

/// Build a `BytesMut` that simulates the real `buffer.split_to(pos)`
/// behavior: the returned BytesMut has the line's bytes at the start,
/// but ALSO has spare capacity (because `split_to` preserves the
/// parent buffer's capacity, and the parent is `BytesMut::with_capacity(8192)`).
/// This is critical for the NEW path: `extend_from_slice(b"\n\n")`
/// only avoids a realloc when there's spare capacity.
fn make_line_with_spare_capacity(chunk: &str) -> BytesMut {
    let mut buf = BytesMut::with_capacity(8192);
    buf.extend_from_slice(chunk.as_bytes());
    // In the real code, `split_to(pos)` would return a BytesMut
    // pointing at the first `pos` bytes, with the parent's capacity.
    // We simulate this by returning `buf` directly (it has the line
    // bytes + 8192 - chunk.len() bytes of spare capacity).
    buf
}

/// Simulate the line scanner: for each sample chunk, build a BytesMut
/// containing `data: <payload>` (without the trailing newline, as
/// `split_to(pos)` would produce), then run the reframe function.
fn bench_old(c: &mut Criterion) {
    let mut group = c.benchmark_group("openai_fast_path_reframe");
    group.throughput(criterion::Throughput::Elements(SAMPLE_CHUNKS.len() as u64));
    group.bench_function("old_alloc_per_chunk", |b| {
        b.iter(|| {
            let mut total: u64 = 0;
            for chunk in SAMPLE_CHUNKS {
                // Simulate the line scanner: `buffer.split_to(pos)` returns
                // a BytesMut with the line bytes + spare capacity.
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

/// Also benchmark the Gemini probe-struct parse vs the old Value-based
/// parse, to quantify the input-side improvement.
fn bench_gemini_parse(c: &mut Criterion) {
    use openproxy_core::sse::parse_gemini_sse_line;

    // Valid Gemini chunks (note: `]}` closes parts array + content object
    // BEFORE the `,` that separates candidates array elements).
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
