use criterion::{Criterion, criterion_group, criterion_main};
use openproxy_core::free_proxies::{ScrapedProxy, upsert_scraped_proxies};

fn benchmark_upsert(c: &mut Criterion) {
    let tmp_dir = tempfile::tempdir().unwrap();
    let db_path = tmp_dir.path().join("test.db");
    let pool = openproxy_db::conn::DbPool::open(&db_path).unwrap();
    let mut conn = pool.writer();
    openproxy_db::migrations::run(&mut conn).unwrap();

    let mut proxies = Vec::new();
    for i in 0..500 {
        proxies.push(ScrapedProxy {
            source: "test".to_string(),
            host: format!("127.0.0.{}", i),
            port: 8080,
            r#type: "http".to_string(),
            country_code: Some("US".to_string()),
            username: None,
            password: None,
            priority: 0,
        });
    }

    c.bench_function("upsert 500 proxies", |b| {
        b.iter(|| {
            upsert_scraped_proxies(&mut conn, &proxies).unwrap();
        })
    });
}

criterion_group!(benches, benchmark_upsert);
criterion_main!(benches);
