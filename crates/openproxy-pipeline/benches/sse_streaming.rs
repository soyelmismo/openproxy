use criterion::{Criterion, black_box, criterion_group, criterion_main};
use openproxy_pipeline::sse::{
    AnthropicToolUseAccumulator, parse_gemini_sse_line, parse_openai_sse_line,
    translate_anthropic_sse_event,
};

fn bench_openai_sse_line(c: &mut Criterion) {
    let line = r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":0,"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":null}]}"#;
    c.bench_function("openai_sse_line_parse", |b| {
        b.iter(|| parse_openai_sse_line(black_box(line)).expect("failed to parse openai sse"))
    });
}

fn bench_gemini_sse_line(c: &mut Criterion) {
    let line = r#"data: {"candidates":[{"content":{"parts":[{"text":"Hello"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}"#;
    c.bench_function("gemini_sse_line_parse", |b| {
        b.iter(|| {
            parse_gemini_sse_line(
                black_box(line),
                black_box("test-id"),
                black_box(0),
                black_box("gemini-pro"),
            )
            .expect("failed to parse gemini sse")
        })
    });
}

fn bench_anthropic_sse_streaming(c: &mut Criterion) {
    let payload = r#"content_block_delta
{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#;
    let chunk_id = "test-id";
    let created = 0;
    let model = "claude-3";

    c.bench_function("anthropic_sse_event_translation", |b| {
        b.iter(|| {
            let mut acc: Option<AnthropicToolUseAccumulator> = None;
            let mut counter: u32 = 0;
            translate_anthropic_sse_event(
                black_box(payload),
                black_box(chunk_id),
                black_box(created),
                black_box(model),
                black_box(&mut acc),
                black_box(&mut counter),
            )
            .expect("failed to translate anthropic sse event")
        })
    });
}

criterion_group!(
    benches,
    bench_openai_sse_line,
    bench_gemini_sse_line,
    bench_anthropic_sse_streaming
);
criterion_main!(benches);
