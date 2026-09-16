use super::*;
use crate::models::{delete, get_by_row_id, list_all, upsert_many};
use openproxy_types::TargetFormat;

fn seed_test_combo_and_targets(conn: &Connection, m_id: i64, m2_id: i64) -> i64 {
    conn.execute_batch(
        "CREATE TABLE combos (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, \
                                   strategy TEXT NOT NULL, race_size INTEGER NOT NULL DEFAULT 1); \
             CREATE TABLE combo_targets (id INTEGER PRIMARY KEY AUTOINCREMENT, \
                                           combo_id INTEGER NOT NULL, provider_id TEXT NOT NULL, \
                                           account_id INTEGER, sub_combo_id INTEGER, \
                                           model_row_id INTEGER \
                                           REFERENCES models(id) ON DELETE CASCADE);",
    )
    .expect("combo tables");
    let combo_id: i64 = conn
        .execute(
            "INSERT INTO combos(name, strategy) VALUES ('c', 'priority')",
            [],
        )
        .expect("insert combo") as i64;
    conn.execute(
        "INSERT INTO combo_targets(combo_id, provider_id, model_row_id) \
             VALUES (?1, ?2, ?3)",
        rusqlite::params![combo_id, "provA", m_id],
    )
    .expect("insert T");
    conn.execute(
        "INSERT INTO combo_targets(combo_id, provider_id, model_row_id) \
             VALUES (?1, ?2, ?3)",
        rusqlite::params![combo_id, "provA", m2_id],
    )
    .expect("insert T2");
    combo_id
}

fn query_combo_target_model_row_id(
    conn: &Connection,
    combo_id: i64,
    model_row_id: i64,
) -> Option<i64> {
    conn.query_row(
        "SELECT model_row_id FROM combo_targets WHERE combo_id = ?1 \
         AND provider_id = ?2 AND model_row_id = ?3",
        rusqlite::params![combo_id, "provA", model_row_id],
        |r| r.get(0),
    )
    .ok()
}

fn count_targets_for(conn: &Connection, combo_id: i64) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM combo_targets WHERE combo_id = ?1",
        [combo_id],
        |r| r.get(0),
    )
    .expect("count targets")
}

#[test]
fn delete_model_nulls_combo_target_model_row_id() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");

    upsert_many(
        &conn,
        &provider,
        &[
            discovered("m1", TargetFormat::Openai),
            discovered("m2", TargetFormat::Anthropic),
        ],
        Duration::from_hours(1),
    )
    .expect("seed");
    let all = list_all(&conn).unwrap();
    let m1_id = all
        .iter()
        .find(|m| m.model_id.as_str() == "m1")
        .unwrap()
        .row_id;
    let m2_id = all
        .iter()
        .find(|m| m.model_id.as_str() == "m2")
        .unwrap()
        .row_id;

    conn.execute_batch(
        "CREATE TABLE combos (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, \
                                   strategy TEXT NOT NULL, race_size INTEGER NOT NULL DEFAULT 1); \
             CREATE TABLE combo_targets (id INTEGER PRIMARY KEY AUTOINCREMENT, \
                                           combo_id INTEGER NOT NULL, provider_id TEXT NOT NULL, \
                                           account_id INTEGER, sub_combo_id INTEGER, \
                                           model_row_id INTEGER \
                                           REFERENCES models(id) ON DELETE CASCADE);",
    )
    .expect("combo tables");
    let combo_id: i64 = conn
        .execute(
            "INSERT INTO combos(name, strategy) VALUES ('c', 'priority')",
            [],
        )
        .expect("insert combo") as i64;
    conn.execute(
        "INSERT INTO combo_targets(combo_id, provider_id, model_row_id) \
             VALUES (?1, ?2, ?3)",
        rusqlite::params![combo_id, "provA", m1_id.0],
    )
    .expect("insert target");

    let pre_targets: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM combo_targets WHERE model_row_id = ?1",
            [m1_id.0],
            |r| r.get(0),
        )
        .expect("count targets");
    assert_eq!(pre_targets, 1);

    let removed = delete(&conn, m1_id).expect("delete m1");
    assert_eq!(removed, 1, "one row removed");

    assert!(get_by_row_id(&conn, m1_id).unwrap().is_none(), "m1 gone");
    assert!(get_by_row_id(&conn, m2_id).unwrap().is_some(), "m2 alive");
    let count_targets: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM combo_targets WHERE combo_id = ?1",
            [combo_id],
            |r| r.get(0),
        )
        .expect("count targets");
    assert_eq!(count_targets, 0);

    let removed_again = delete(&conn, m1_id).expect("delete again");
    assert_eq!(removed_again, 0, "missing id is a no-op");
}

#[test]
fn delete_model_sets_combo_target_model_row_id_to_null() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");

    upsert_many(
        &conn,
        &provider,
        &[
            discovered("m-to-delete", TargetFormat::Openai),
            discovered("m-keep", TargetFormat::Anthropic),
        ],
        Duration::from_hours(1),
    )
    .expect("seed");
    let all = list_all(&conn).unwrap();
    let m_id = all
        .iter()
        .find(|m| m.model_id.as_str() == "m-to-delete")
        .unwrap()
        .row_id;
    let m2_id = all
        .iter()
        .find(|m| m.model_id.as_str() == "m-keep")
        .unwrap()
        .row_id;

    let combo_id = seed_test_combo_and_targets(&conn, m_id.0, m2_id.0);

    assert_eq!(count_targets_for(&conn, combo_id), 2);
    assert_eq!(
        query_combo_target_model_row_id(&conn, combo_id, m_id.0),
        Some(m_id.0)
    );
    assert_eq!(
        query_combo_target_model_row_id(&conn, combo_id, m2_id.0),
        Some(m2_id.0)
    );

    let removed = delete(&conn, m_id).expect("delete M");
    assert_eq!(removed, 1);

    assert!(get_by_row_id(&conn, m_id).unwrap().is_none());
    assert!(get_by_row_id(&conn, m2_id).unwrap().is_some());

    assert_eq!(
        query_combo_target_model_row_id(&conn, combo_id, m_id.0),
        None
    );
    assert_eq!(
        query_combo_target_model_row_id(&conn, combo_id, m2_id.0),
        Some(m2_id.0)
    );
    assert_eq!(count_targets_for(&conn, combo_id), 1);
}
