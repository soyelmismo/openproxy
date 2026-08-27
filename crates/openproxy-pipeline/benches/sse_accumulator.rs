use criterion::{Criterion, black_box, criterion_group, criterion_main};
use openproxy_pipeline::sse_accumulator::{
    ResponseAccumulator, extract_reasoning_content, normalize_nonstandard_reasoning_fields,
};

fn bench_accumulator_append_openai_raw(c: &mut Criterion) {
    let payload = r#"{"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hello world! This is a longer stream chunk to simulate actual accumulation overhead."},"finish_reason":null}]}"#;
    c.bench_function("accumulator_append_openai_raw", |b| {
        b.iter(|| {
            let mut acc = ResponseAccumulator::new();
            acc.append_openai_raw(black_box(payload));
        })
    });
}

fn bench_accumulator_extract_reasoning_content(c: &mut Criterion) {
    let payload = r#"{"choices":[{"delta":{"reasoning_content":"Thinking process goes here."}}]}"#;
    c.bench_function("accumulator_extract_reasoning_content", |b| {
        b.iter(|| extract_reasoning_content(black_box(payload)))
    });
}

fn bench_accumulator_normalize_nonstandard_reasoning(c: &mut Criterion) {
    let payload = r#"{"id":"x","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"content":"","role":"assistant","reasoning":" Need","reasoning_details":[{"type":"reasoning.text","text":" Need","format":"unknown","index":0}]},"finish_reason":null}]}"#;
    c.bench_function("accumulator_normalize_nonstandard_reasoning", |b| {
        b.iter(|| normalize_nonstandard_reasoning_fields(black_box(payload)))
    });
}

fn bench_accumulator_extract_upstream_error_from_raw(c: &mut Criterion) {
    let payload = r#"data: {"id":"gen-123","choices":[],"error":{"code":502,"message":"Upstream error from Nvidia"}}"#;
    c.bench_function("accumulator_extract_upstream_error", |b| {
        let mut acc = ResponseAccumulator::new();
        acc.append_raw_line(payload);
        b.iter(|| acc.extract_upstream_error_from_raw())
    });
}

fn bench_accumulator_finish(c: &mut Criterion) {
    c.bench_function("accumulator_finish", |b| {
        let mut acc = ResponseAccumulator::new();
        for _ in 0..100 {
            acc.append_openai_raw(r#"{"choices":[{"delta":{"content":"word "}}]}"#);
        }
        b.iter(|| {
            acc.finish(
                black_box("test_id"),
                black_box(12345),
                black_box("test_model"),
            )
        })
    });
}

criterion_group!(
    benches,
    bench_accumulator_append_openai_raw,
    bench_accumulator_extract_reasoning_content,
    bench_accumulator_normalize_nonstandard_reasoning,
    bench_accumulator_extract_upstream_error_from_raw,
    bench_accumulator_finish,
);
criterion_main!(benches);
