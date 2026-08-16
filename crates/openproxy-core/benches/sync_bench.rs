use criterion::{Criterion, criterion_group, criterion_main};
use openproxy_core::models::sync::{SyncDiff, generate_events};
use openproxy_types::ids::ProviderId;
use rusqlite::Connection;

fn bench_generate_events(c: &mut Criterion) {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute(
        "CREATE TABLE notifications (
            id INTEGER PRIMARY KEY,
            kind TEXT NOT NULL,
            payload_json TEXT NOT NULL,
            dedup_key TEXT,
            provider_id TEXT,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )",
        [],
    )
    .unwrap();
    conn.execute(
        "CREATE UNIQUE INDEX idx_notifications_dedup ON notifications (kind, dedup_key, date(created_at)) WHERE dedup_key IS NOT NULL",
        []
    ).unwrap();

    let provider = ProviderId::new("test_provider");

    c.bench_function("generate_events_deleted_models", |b| {
        b.iter(|| {
            let tx = conn.transaction().unwrap();

            let diff = SyncDiff {
                discovered_set: std::collections::HashSet::new(),
                new_models: vec![],
                existing_rows: {
                    let mut m = Vec::new();
                    for i in 0..1000 {
                        m.push((
                            format!("model_{i}"),
                            i64::from(i),
                            Some(format!("Model {i}")),
                        ));
                    }
                    m
                },
            };

            let _ = generate_events(&tx, &provider, &diff).unwrap();

            tx.rollback().unwrap();
        });
    });
}

criterion_group!(benches, bench_generate_events);
criterion_main!(benches);
