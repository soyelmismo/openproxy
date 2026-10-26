#![allow(clippy::expect_used, clippy::unwrap_used)]

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use openproxy_pipeline::sse::{
    AnthropicToolUseAccumulator, parse_gemini_sse_line, parse_openai_sse_line,
    translate_anthropic_sse_event,
};
use openproxy_pipeline::sse_accumulator::ResponseAccumulator;
use std::hint::black_box;

/// Generates a synthetic stream of N tokens in OpenAI SSE format.
fn generate_openai_tokens(token_count: usize) -> Vec<String> {
    (0..token_count)
        .map(|i| {
            format!(
                r#"data: {{"id":"bench-stream","object":"chat.completion.chunk","created":1700000000,"model":"bench-model","choices":[{{"index":0,"delta":{{"content":" tok_{i}"}},"finish_reason":null}}]}}"#
            )
        })
        .collect()
}

/// Generates a synthetic stream of N tokens in Anthropic SSE event format.
fn generate_anthropic_tokens(token_count: usize) -> Vec<String> {
    (0..token_count)
        .map(|i| {
            format!(
                "content_block_delta\n{{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\" tok_{i}\"}}}}"
            )
        })
        .collect()
}

/// Generates a synthetic stream of N tokens in Gemini SSE candidate format.
fn generate_gemini_tokens(token_count: usize) -> Vec<String> {
    (0..token_count)
        .map(|i| {
            format!(
                r#"data: {{"candidates":[{{"content":{{"parts":[{{"text":" tok_{i}"}}]}},"finishReason":null}}]}}"#
            )
        })
        .collect()
}

fn bench_openai_token_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("openai_token_throughput");
    // Scale token batch sizes across standard agent context steps
    for &tokens in &[1_000, 10_000, 50_000] {
        let stream = generate_openai_tokens(tokens);
        group.throughput(Throughput::Elements(tokens as u64));

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{tokens}_tokens")),
            &stream,
            |b, stream_data| {
                b.iter(|| {
                    let mut parsed_count = 0usize;
                    for line in stream_data {
                        if let Ok(Some(_)) = parse_openai_sse_line(black_box(line)) {
                            parsed_count += 1;
                        }
                    }
                    assert_eq!(parsed_count, tokens);
                })
            },
        );
    }
    group.finish();
}

fn bench_anthropic_token_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("anthropic_token_throughput");
    for &tokens in &[1_000, 10_000, 50_000] {
        let stream = generate_anthropic_tokens(tokens);
        group.throughput(Throughput::Elements(tokens as u64));

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{tokens}_tokens")),
            &stream,
            |b, stream_data| {
                b.iter(|| {
                    let mut acc: Option<AnthropicToolUseAccumulator> = None;
                    let mut counter: u32 = 0;
                    let mut translated = 0usize;
                    for event in stream_data {
                        if let Ok(Some(_)) = translate_anthropic_sse_event(
                            black_box(event),
                            black_box("chunk-id"),
                            black_box(0),
                            black_box("claude-bench"),
                            black_box(&mut acc),
                            black_box(&mut counter),
                        ) {
                            translated += 1;
                        }
                    }
                    assert_eq!(translated, tokens);
                })
            },
        );
    }
    group.finish();
}

fn bench_gemini_token_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("gemini_token_throughput");
    for &tokens in &[1_000, 10_000, 50_000] {
        let stream = generate_gemini_tokens(tokens);
        group.throughput(Throughput::Elements(tokens as u64));

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{tokens}_tokens")),
            &stream,
            |b, stream_data| {
                b.iter(|| {
                    let mut counter: u32 = 0;
                    let mut parsed = 0usize;
                    for line in stream_data {
                        if let Ok(Some(_)) = parse_gemini_sse_line(
                            black_box(line),
                            black_box("gemini-chunk"),
                            black_box(0),
                            black_box("gemini-bench"),
                            black_box(&mut counter),
                        ) {
                            parsed += 1;
                        }
                    }
                    assert_eq!(parsed, tokens);
                })
            },
        );
    }
    group.finish();
}

fn bench_accumulator_high_token_scale(c: &mut Criterion) {
    let mut group = c.benchmark_group("accumulator_token_scale");
    // Measure response accumulation for large responses (1k up to 25k tokens in memory)
    for &tokens in &[1_000, 5_000, 20_000] {
        let payloads: Vec<String> = (0..tokens)
            .map(|i| format!(r#"{{"choices":[{{"delta":{{"content":" word_{i}"}}}}]}}"#))
            .collect();

        group.throughput(Throughput::Elements(tokens as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{tokens}_tokens")),
            &payloads,
            |b, batch| {
                b.iter(|| {
                    let mut acc = ResponseAccumulator::new();
                    for chunk in batch {
                        acc.append_openai_raw(black_box(chunk));
                    }
                    let res = acc.finish("bench_id", 1700000000, "bench_model");
                    assert!(res.is_object());
                })
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_openai_token_throughput,
    bench_anthropic_token_throughput,
    bench_gemini_token_throughput,
    bench_accumulator_high_token_scale,
);
criterion_main!(benches);
