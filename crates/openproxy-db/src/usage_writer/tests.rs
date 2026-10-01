use super::*;
use openproxy_types::{EndpointKind, ProviderId, RequestId};

fn input() -> UsageInput {
    UsageInput {
        request_id: RequestId::new(),
        trace_id: "test".into(),
        attempt: 1,
        provider_id: ProviderId::new("test"),
        account_id: None,
        combo_id: None,
        combo_target_id: None,
        model_row_id: None,
        upstream_model_id: "test".into(),
        prompt_tokens: None,
        completion_tokens: None,
        cached_tokens: None,
        connect_ms: None,
        ttft_ms: None,
        total_ms: 10,
        status_code: 200,
        error_msg: None,
        race_total: 1,
        api_key_id: None,
        request_body_json: None,
        response_body_json: None,
        request_headers: None,
        response_headers: None,
        error_message: None,
        race_attempts: 1,
        stop_reason: None,
        compression_savings_pct: None,
        compression_techniques: None,
        pii_redacted: None,
        endpoint_kind: EndpointKind::Chat,
        proxy_url: None,
        proxy_status: None,
        flags: 0,
    }
}

fn cooldown() -> AttemptCooldown<'static> {
    AttemptCooldown {
        target_id: ComboTargetId(42),
        combo_id: ComboId(1),
        error: None,
        is_upstream_health_issue: false,
        mode: CooldownMode::Flat,
        base_secs: 60,
        max_secs: 3600,
        factor: 2,
    }
}

fn connection() -> Connection {
    let conn = crate::testing::open_in_memory();
    conn.execute_batch(
        "INSERT INTO providers (id, name, base_url, auth_type, format)
         VALUES ('test', 'test', 'https://example.com', 'none', 'openai');
         INSERT INTO combos (id, name, strategy) VALUES (1, 'test', 'priority');
         INSERT INTO combo_targets (id, combo_id, provider_id, priority_order)
         VALUES (42, 1, 'test', 0);",
    )
    .unwrap();
    conn
}

#[test]
fn usage_is_rolled_back_if_cooldown_write_fails() {
    let mut conn = connection();
    conn.execute_batch(
        "CREATE TRIGGER fail_clear BEFORE DELETE ON target_cooldowns
         BEGIN SELECT RAISE(ABORT, 'cooldown write failed'); END;
         INSERT INTO target_cooldowns (combo_target_id, cooldown_until, failure_count)
         VALUES (42, datetime('now', '+1 minute'), 1);",
    )
    .unwrap();
    assert!(record(&mut conn, &input(), Some(&cooldown())).is_err());
    let count: i64 = conn
        .query_row("SELECT count(*) FROM usage", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
    let count: i64 = conn
        .query_row("SELECT count(*) FROM target_cooldowns", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 1);
    assert!(conn.is_autocommit());
}

#[test]
fn successful_usage_and_cooldown_clear_commit_together() {
    let mut conn = connection();
    conn.execute(
        "INSERT INTO target_cooldowns (combo_target_id, cooldown_until, failure_count)
         VALUES (42, datetime('now', '+1 minute'), 1)",
        [],
    )
    .unwrap();
    let (id, row) = record(&mut conn, &input(), Some(&cooldown())).unwrap();
    assert_eq!(id, row.id);
    let count: i64 = conn
        .query_row("SELECT count(*) FROM usage", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    let count: i64 = conn
        .query_row("SELECT count(*) FROM target_cooldowns", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn synthetic_target_does_not_change_persistent_cooldowns() {
    let mut conn = connection();
    conn.execute(
        "INSERT INTO target_cooldowns (combo_target_id, cooldown_until, failure_count)
         VALUES (42, datetime('now', '+1 minute'), 1)",
        [],
    )
    .unwrap();
    let mut params = cooldown();
    params.combo_id = ComboId(-1);
    record(&mut conn, &input(), Some(&params)).unwrap();
    let count: i64 = conn
        .query_row("SELECT count(*) FROM target_cooldowns", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn journal_acknowledgement_is_atomic_and_replay_is_idempotent() {
    let mut conn = connection();
    let id = crate::usage_journal::append(&mut conn, "payload").unwrap();
    assert_eq!(crate::usage_journal::depth(&conn).unwrap(), 1);
    assert!(
        record_journaled(&mut conn, &input(), None, Some(id))
            .unwrap()
            .is_some()
    );
    assert_eq!(crate::usage_journal::depth(&conn).unwrap(), 0);
    assert!(
        record_journaled(&mut conn, &input(), None, Some(id))
            .unwrap()
            .is_none()
    );
    let count: i64 = conn
        .query_row("SELECT count(*) FROM usage", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn failed_apply_preserves_journal_for_restart() {
    let mut conn = connection();
    let id = crate::usage_journal::append(&mut conn, "payload").unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_usage BEFORE INSERT ON usage BEGIN SELECT RAISE(ABORT, 'full'); END;",
    )
    .unwrap();
    assert!(record_journaled(&mut conn, &input(), None, Some(id)).is_err());
    assert_eq!(crate::usage_journal::depth(&conn).unwrap(), 1);
    conn.execute_batch("DROP TRIGGER fail_usage").unwrap();
    assert!(
        record_journaled(&mut conn, &input(), None, Some(id))
            .unwrap()
            .is_some()
    );
    assert_eq!(crate::usage_journal::depth(&conn).unwrap(), 0);
}
