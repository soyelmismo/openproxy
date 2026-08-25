use criterion::{Criterion, black_box, criterion_group, criterion_main};
use openproxy_pipeline::think_extractor::ThinkStreamExtractor;

fn bench_think_extractor_streaming(c: &mut Criterion) {
    let standard_payload =
        "<think>Let me think about this step by step.\n1. A\n2. B</think>The final answer is here.";
    c.bench_function("think_extractor_standard", |b| {
        b.iter(|| {
            let mut ext = ThinkStreamExtractor::new();
            ext.process(black_box(standard_payload));
            ext.flush();
        })
    });

    let split_chunks = vec![
        "<thi",
        "nk>This is reasoning",
        " which is split across</t",
        "hink>And here is the answer",
    ];
    c.bench_function("think_extractor_split_chunks", |b| {
        b.iter(|| {
            let mut ext = ThinkStreamExtractor::new();
            for chunk in &split_chunks {
                ext.process(black_box(chunk));
            }
            ext.flush();
        })
    });

    let long_reasoning = format!(
        "<think>{}</think>Finally.",
        "A long time ago in a galaxy far far away...".repeat(100)
    );
    c.bench_function("think_extractor_long", |b| {
        b.iter(|| {
            let mut ext = ThinkStreamExtractor::new();
            ext.process(black_box(&long_reasoning));
            ext.flush();
        })
    });
}

criterion_group!(benches, bench_think_extractor_streaming);
criterion_main!(benches);
