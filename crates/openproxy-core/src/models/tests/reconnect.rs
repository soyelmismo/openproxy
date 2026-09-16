use super::*;
use crate::models::{get_by_row_id, list_all, upsert_many};
use openproxy_types::TargetFormat;

/// Schema setup for combo_targets reconnect tests.
fn seed_combo_targets_schema(conn: &Connection) -> i64 {
    conn.execute_batch(
        "CREATE TABLE accounts (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 provider_id TEXT NOT NULL REFERENCES providers(id),
                 label       TEXT NOT NULL,
                 auth_kind   TEXT NOT NULL,
                 created_at  TEXT NOT NULL DEFAULT (datetime('now'))
             );
             CREATE TABLE combos (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 name       TEXT NOT NULL,
                 strategy   TEXT NOT NULL,
                 race_size  INTEGER NOT NULL DEFAULT 1
             );
             CREATE TABLE combo_targets (
                 id                INTEGER PRIMARY KEY AUTOINCREMENT,
                 combo_id          INTEGER NOT NULL REFERENCES combos(id) ON DELETE CASCADE,
                 provider_id       TEXT NOT NULL REFERENCES providers(id),
                 account_id        INTEGER REFERENCES accounts(id),
                 model_row_id      INTEGER REFERENCES models(id) ON DELETE CASCADE,
                 sub_combo_id      INTEGER REFERENCES combos(id) ON DELETE CASCADE,
                 upstream_model_id TEXT,
                 priority_order    INTEGER NOT NULL,
                 UNIQUE(combo_id, account_id, model_row_id),
                 CHECK (NOT (model_row_id IS NOT NULL AND sub_combo_id IS NOT NULL))
             );",
    )
    .expect("combo schema");
    conn.execute(
        "INSERT INTO combos(name, strategy) VALUES ('c1', 'priority')",
        [],
    )
    .expect("insert combo");
    conn.query_row("SELECT id FROM combos ORDER BY id DESC LIMIT 1", [], |r| {
        r.get::<_, i64>(0)
    })
    .expect("read combo id")
}

fn read_target_row(conn: &Connection, combo_id: i64) -> (Option<i64>, Option<String>) {
    conn.query_row(
        "SELECT model_row_id, upstream_model_id \
             FROM combo_targets WHERE combo_id = ?1",
        [combo_id],
        |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<String>>(1)?)),
    )
    .expect("read target row")
}

#[test]
#[ignore = "Gate F1 reconnect path is dead code under migration 000030 \
                (ON DELETE CASCADE removes combo_targets rows with their model); \
                the reconnect logic in upsert_many is retained for forward-compat"]
fn upsert_many_reconnects_orphan_combo_targets() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    let combo_id = seed_combo_targets_schema(&conn);

    upsert_many(
        &conn,
        &provider,
        &[discovered("m1", TargetFormat::Openai)],
        Duration::from_hours(1),
    )
    .expect("seed m1");
    let m1_row_id_v1 = list_all(&conn)
        .unwrap()
        .into_iter()
        .find(|m| m.model_id.as_str() == "m1")
        .map(|m| m.row_id)
        .expect("m1 row");

    conn.execute(
        "INSERT INTO combo_targets(combo_id, provider_id, model_row_id, upstream_model_id, priority_order) \
         VALUES (?1, ?2, ?3, ?4, 0)",
        rusqlite::params![combo_id, "provA", m1_row_id_v1.0, "m1"],
    )
    .expect("insert target");

    upsert_many(
        &conn,
        &provider,
        &[discovered("m2", TargetFormat::Openai)],
        Duration::from_hours(1),
    )
    .expect("upsert without m1");

    assert!(
        get_by_row_id(&conn, m1_row_id_v1).unwrap().is_none(),
        "m1 hard-deleted"
    );
    let (orphan_fk, orphan_up) = read_target_row(&conn, combo_id);
    assert!(orphan_fk.is_none(), "target.model_row_id nulled by FK");
    assert_eq!(orphan_up.as_deref(), Some("m1"));

    upsert_many(
        &conn,
        &provider,
        &[discovered("m1", TargetFormat::Openai)],
        Duration::from_hours(1),
    )
    .expect("re-upsert m1");

    let m1_row_id_v2 = list_all(&conn)
        .unwrap()
        .into_iter()
        .find(|m| m.model_id.as_str() == "m1")
        .map(|m| m.row_id)
        .expect("m1 row again");
    assert_ne!(m1_row_id_v1, m1_row_id_v2);

    let (rebound_fk, rebound_up) = read_target_row(&conn, combo_id);
    assert_eq!(rebound_fk, Some(m1_row_id_v2.0));
    assert_eq!(rebound_up.as_deref(), Some("m1"));
    let row_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM combo_targets WHERE combo_id = ?1",
            [combo_id],
            |r| r.get(0),
        )
        .expect("count targets");
    assert_eq!(row_count, 1);
}

#[test]
#[ignore = "Gate F1 reconnect path is dead code under migration 000030 \
                (ON DELETE CASCADE removes combo_targets rows with their model); \
                the reconnect logic in upsert_many is retained for forward-compat"]
fn upsert_many_does_not_reconnect_wrong_model() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    let combo_id = seed_combo_targets_schema(&conn);

    upsert_many(
        &conn,
        &provider,
        &[discovered("m_a", TargetFormat::Openai)],
        Duration::from_hours(1),
    )
    .expect("seed m_a");
    let m_a_v1 = list_all(&conn)
        .unwrap()
        .into_iter()
        .find(|m| m.model_id.as_str() == "m_a")
        .map(|m| m.row_id)
        .expect("m_a row");

    conn.execute(
        "INSERT INTO combo_targets(combo_id, provider_id, model_row_id, upstream_model_id, priority_order) \
         VALUES (?1, ?2, ?3, ?4, 0)",
        rusqlite::params![combo_id, "provA", m_a_v1.0, "m_a"],
    )
    .expect("insert target");

    upsert_many(&conn, &provider, &[], Duration::from_hours(1)).expect("upsert empty");
    let (orphan_fk, orphan_up) = read_target_row(&conn, combo_id);
    assert!(orphan_fk.is_none());
    assert_eq!(orphan_up.as_deref(), Some("m_a"));

    upsert_many(
        &conn,
        &provider,
        &[discovered("m_b", TargetFormat::Openai)],
        Duration::from_hours(1),
    )
    .expect("seed m_b");
    let m_b_id = list_all(&conn)
        .unwrap()
        .into_iter()
        .find(|m| m.model_id.as_str() == "m_b")
        .map(|m| m.row_id)
        .expect("m_b row");

    let (post_fk, post_up) = read_target_row(&conn, combo_id);
    assert!(post_fk.is_none());
    assert_eq!(post_up.as_deref(), Some("m_a"));
    assert_eq!(m_b_id.0, post_fk.unwrap_or(m_b_id.0));
    assert!(get_by_row_id(&conn, m_b_id).unwrap().is_some());
}

#[test]
#[ignore = "Gate F1 reconnect path is dead code under migration 000030 \
                (ON DELETE CASCADE removes combo_targets rows with their model); \
                the reconnect logic in upsert_many is retained for forward-compat"]
fn upsert_many_atomic_orphan_reconnection() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    let combo_id = seed_combo_targets_schema(&conn);

    upsert_many(
        &conn,
        &provider,
        &[discovered("m1", TargetFormat::Openai)],
        Duration::from_hours(1),
    )
    .expect("seed m1");
    let m1_v1 = list_all(&conn)
        .unwrap()
        .into_iter()
        .find(|m| m.model_id.as_str() == "m1")
        .map(|m| m.row_id)
        .expect("m1");
    conn.execute(
        "INSERT INTO combo_targets(combo_id, provider_id, model_row_id, upstream_model_id, priority_order) \
         VALUES (?1, ?2, ?3, ?4, 0)",
        rusqlite::params![combo_id, "provA", m1_v1.0, "m1"],
    )
    .expect("insert target");

    upsert_many(&conn, &provider, &[], Duration::from_hours(1)).expect("upsert empty");
    let (orphan_fk, _orphan_up) = read_target_row(&conn, combo_id);
    assert!(orphan_fk.is_none());

    conn.execute_batch(
        "CREATE TRIGGER fail_m1_reinsert \
             BEFORE INSERT ON models \
             WHEN NEW.model_id = 'm1' \
             BEGIN \
                 SELECT RAISE(ABORT, 'simulated failure: m1 re-insert blocked'); \
             END;",
    )
    .expect("install failure trigger");

    let result = upsert_many(
        &conn,
        &provider,
        &[discovered("m1", TargetFormat::Openai)],
        Duration::from_hours(1),
    );
    assert!(result.is_err());

    let (post_fk, post_up) = read_target_row(&conn, combo_id);
    assert!(post_fk.is_none());
    assert_eq!(post_up.as_deref(), Some("m1"));

    assert!(
        list_all(&conn)
            .unwrap()
            .iter()
            .all(|m| m.model_id.as_str() != "m1")
    );

    conn.execute_batch("DROP TRIGGER fail_m1_reinsert;")
        .expect("drop trigger");
    upsert_many(
        &conn,
        &provider,
        &[discovered("m1", TargetFormat::Openai)],
        Duration::from_hours(1),
    )
    .expect("clean re-upsert");
    let m1_v2 = list_all(&conn)
        .unwrap()
        .into_iter()
        .find(|m| m.model_id.as_str() == "m1")
        .map(|m| m.row_id)
        .expect("m1 v2");
    let (final_fk, final_up) = read_target_row(&conn, combo_id);
    assert_eq!(final_fk, Some(m1_v2.0));
    assert_eq!(final_up.as_deref(), Some("m1"));
}
