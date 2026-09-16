use super::*;
use crate::conn::DbPool;
use crate::providers::{self, NewProvider};
use openproxy_types::error::CoreError;
use openproxy_types::{
    AuthType, DiscoveredModel, ModelId, ProviderFormat, ProviderId as CoreProviderId,
    RateLimitScope, TargetFormat,
};
use rusqlite::Connection;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn fresh_pool() -> (DbPool, PathBuf) {
    let pool = DbPool::test_pool_with_prefix("openproxy-models-test").expect("open pool");
    let path = pool.path().to_path_buf();
    (pool, path)
}

fn seed_provider(conn: &Connection, provider: &CoreProviderId) {
    providers::create(
        conn,
        NewProvider {
            id: provider,
            name: provider.as_str(),
            base_url: "https://example.invalid",
            auth_type: AuthType::Bearer,
            format: ProviderFormat::Openai,
            extra_headers_json: None,
            auto_activate_keyword: None,
            rate_limit_scope: RateLimitScope::Account,
        },
    )
    .expect("seed provider");
}

fn seed_models(conn: &Connection, provider: &CoreProviderId, ids: &[&str]) {
    let models = ids
        .iter()
        .map(|id| DiscoveredModel {
            model_id: ModelId::new(*id),
            display_name: Some((*id).to_string()),
            target_format: TargetFormat::Openai,
            context_length: None,
            max_output_tokens: None,
            input_modalities: None,
            output_modalities: None,
            model_type: None,
            family: None,
            capabilities: None,
        })
        .collect::<Vec<_>>();
    upsert_many(conn, provider, &models, Duration::from_hours(1)).expect("seed models");
}

#[test]
fn apply_auto_activation_with_retry_succeeds_on_first_attempt() {
    let (pool, _path) = fresh_pool();
    let conn = pool.open_connection().unwrap();
    let provider = CoreProviderId::new("acme_ok");
    seed_provider(&conn, &provider);
    seed_models(&conn, &provider, &["gpt-4", "claude-3", "llama-3"]);

    let started = Instant::now();
    let updated = apply_auto_activation_with_retry(&conn, &provider, Some("gpt")).unwrap();
    assert!(updated >= 1);
    assert!(started.elapsed() < Duration::from_millis(40));
}

#[test]
fn apply_auto_activation_with_retry_succeeds_after_transient_busy() {
    let (pool, blocker_path) = fresh_pool();
    let conn = pool.open_connection().unwrap();
    conn.pragma_update(None, "busy_timeout", 0i64).unwrap();
    let provider = CoreProviderId::new("acme_busy");
    seed_provider(&conn, &provider);
    seed_models(&conn, &provider, &["gpt-4", "claude-3"]);

    let (tx_ready, rx_ready) = std::sync::mpsc::channel();
    let blocker = std::thread::spawn(move || {
        let bconn = Connection::open(&blocker_path).unwrap();
        bconn.pragma_update(None, "busy_timeout", 0i64).unwrap();
        let tx = bconn.unchecked_transaction().unwrap();
        tx.execute("INSERT INTO providers (id, name, base_url, auth_type, format) VALUES ('blocker', 'b', 'https://x', 'bearer', 'openai')", []).unwrap();
        let _ = tx_ready.send(());
        std::thread::sleep(Duration::from_millis(25));
        tx.commit().unwrap();
    });
    let _ = rx_ready.recv();

    let started = Instant::now();
    let result = apply_auto_activation_with_retry(&conn, &provider, Some("gpt"));
    let elapsed = started.elapsed();
    blocker.join().unwrap();

    assert!(result.is_ok());
    assert!(elapsed >= Duration::from_millis(30) && elapsed < Duration::from_millis(800));
}

#[test]
fn apply_auto_activation_with_retry_does_not_retry_non_busy_errors() {
    let raw = Connection::open_in_memory().unwrap();
    let provider = CoreProviderId::new("acme_broken");
    let started = Instant::now();
    let result = apply_auto_activation_with_retry(&raw, &provider, Some("gpt"));
    assert!(started.elapsed() < Duration::from_millis(40));
    assert!(matches!(result, Err(CoreError::Database { .. })));
}

fn count_notifs(conn: &Connection, provider: &str) -> (i64, Vec<String>) {
    let mut stmt = conn.prepare("SELECT dedup_key FROM notifications WHERE kind = 'model_auto_activated' AND provider_id = ?1").unwrap();
    let keys = stmt
        .query_map([provider], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>();
    (keys.len() as i64, keys)
}

#[test]
fn notif_keyword_only_toggle_matrix() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let p = CoreProviderId::new("w2_p");
    seed_provider(&conn, &p);

    let candidates = vec![
        ("claude-3".to_string(), Some("Claude 3".to_string())),
        ("gpt-4".to_string(), None),
    ];

    // 1. Toggle ON with keyword -> only matching notifies
    let tx = conn.unchecked_transaction().unwrap();
    notify_auto_activated_models(&tx, &p, Some("claude"), &candidates, true).unwrap();
    tx.commit().unwrap();
    let (cnt1, keys1) = count_notifs(&conn, p.as_str());
    assert_eq!(cnt1, 1);
    assert!(keys1[0].contains("claude-3"));

    // 2. Toggle ON without keyword -> both notify (no-op)
    conn.execute("DELETE FROM notifications", []).unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    notify_auto_activated_models(&tx, &p, None, &candidates, true).unwrap();
    tx.commit().unwrap();
    let (cnt2, _) = count_notifs(&conn, p.as_str());
    assert_eq!(cnt2, 2);

    // 3. Toggle OFF -> both notify
    conn.execute("DELETE FROM notifications", []).unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    notify_auto_activated_models(&tx, &p, Some("claude"), &candidates, false).unwrap();
    tx.commit().unwrap();
    let (cnt3, _) = count_notifs(&conn, p.as_str());
    assert_eq!(cnt3, 2);
}

#[test]
fn notif_keyword_only_end_to_end_applies_toggle_from_provider_row() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let provider = CoreProviderId::new("w2_e2e");
    seed_provider(&conn, &provider);
    seed_models(&conn, &provider, &["claude-3", "gpt-4"]);

    conn.execute(
        "UPDATE models SET active = 0 WHERE provider_id = ?1",
        [provider.as_str()],
    )
    .unwrap();
    conn.execute(
        "UPDATE providers SET notif_keyword_only = 1 WHERE id = ?1",
        [provider.as_str()],
    )
    .unwrap();
    apply_auto_activation(&conn, &provider, Some("claude")).unwrap();

    let (cnt, keys) = count_notifs(&conn, provider.as_str());
    assert_eq!(cnt, 1);
    assert!(keys[0].contains("claude-3") && !keys.iter().any(|k| k.contains("gpt-4")));
    assert!(keys[0].starts_with("w2_e2e:claude-3:auto"));
}
