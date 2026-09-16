use crate::models_dev_sync::*;
use rusqlite::Connection;

#[test]
fn auto_create_combos_appends_new_targets() {
    let conn = Connection::open_in_memory().unwrap();

    conn.execute_batch(
        "CREATE TABLE models (
             id                  INTEGER PRIMARY KEY AUTOINCREMENT,
             provider_id         TEXT NOT NULL,
             model_id            TEXT NOT NULL,
             context_length      INTEGER,
             active              INTEGER NOT NULL DEFAULT 1,
             custom              INTEGER NOT NULL DEFAULT 0,
             model_id_normalized TEXT,
             UNIQUE(provider_id, model_id)
         );
         CREATE TABLE accounts (
             id                  INTEGER PRIMARY KEY AUTOINCREMENT,
             provider_id         TEXT NOT NULL,
             health_status       TEXT NOT NULL DEFAULT 'healthy'
         );
         CREATE TABLE combos (
             id                  INTEGER PRIMARY KEY AUTOINCREMENT,
             name                TEXT NOT NULL UNIQUE,
             strategy            TEXT NOT NULL,
             race_size           INTEGER NOT NULL
         );
         CREATE TABLE combo_targets (
             id                  INTEGER PRIMARY KEY AUTOINCREMENT,
             combo_id            INTEGER NOT NULL REFERENCES combos(id) ON DELETE CASCADE,
             provider_id         TEXT NOT NULL,
             account_id          INTEGER REFERENCES accounts(id),
             model_row_id        INTEGER REFERENCES models(id) ON DELETE CASCADE,
             priority_order      INTEGER NOT NULL,
             UNIQUE(combo_id, account_id, model_row_id)
         );",
    )
    .unwrap();

    // 1. Insert models with different naming conventions that normalize to "gpt-oss-120b"
    conn.execute(
        "INSERT INTO models (provider_id, model_id, model_id_normalized) VALUES ('nvidia-nim', 'openai/gpt-oss-120b', 'gpt-oss-120b')",
        []
    ).unwrap();
    conn.execute(
        "INSERT INTO models (provider_id, model_id, model_id_normalized) VALUES ('groq', 'openai/gpt-oss-120b', 'gpt-oss-120b')",
        []
    ).unwrap();
    conn.execute(
        "INSERT INTO models (provider_id, model_id, model_id_normalized) VALUES ('ollama-cloud', 'gpt-oss:120b', 'gpt-oss-120b')",
        []
    ).unwrap();

    // Insert accounts for these providers (to make them healthy/active)
    conn.execute(
        "INSERT INTO accounts (id, provider_id, health_status) VALUES (1, 'nvidia-nim', 'healthy')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO accounts (id, provider_id, health_status) VALUES (2, 'groq', 'healthy')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO accounts (id, provider_id, health_status) VALUES (3, 'ollama-cloud', 'healthy')",
        [],
    )
    .unwrap();

    // 2. Run auto_create_combos.
    let count = auto_create_combos(&conn).unwrap();
    assert_eq!(count, 1, "Should create 1 auto combo");

    // Verify the combo name and targets
    let combo_id: i64 = conn
        .query_row(
            "SELECT id FROM combos WHERE name = 'auto:gpt-oss-120b'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let targets_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM combo_targets WHERE combo_id = ?1",
            rusqlite::params![combo_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(targets_count, 3, "Should have 3 targets in the combo");

    // Verify priority orders are 0, 1, 2
    let orders: Vec<i32> = {
        let mut stmt = conn
            .prepare("SELECT priority_order FROM combo_targets WHERE combo_id = ?1 ORDER BY priority_order")
            .unwrap();
        let rows = stmt
            .query_map(rusqlite::params![combo_id], |r| r.get::<_, i32>(0))
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    };
    assert_eq!(orders, vec![0, 1, 2]);

    // 3. Insert another model that normalizes to "gpt-oss-120b"
    conn.execute(
        "INSERT INTO models (provider_id, model_id, model_id_normalized) VALUES ('cerebras', 'gpt-oss-120b', 'gpt-oss-120b')",
        []
    ).unwrap();
    conn.execute(
        "INSERT INTO accounts (id, provider_id, health_status) VALUES (4, 'cerebras', 'healthy')",
        [],
    )
    .unwrap();

    // 4. Run auto_create_combos again.
    let count2 = auto_create_combos(&conn).unwrap();
    assert_eq!(count2, 0, "No new combos should be created");

    let targets_count2: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM combo_targets WHERE combo_id = ?1",
            rusqlite::params![combo_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(targets_count2, 4, "Should now have 4 targets in the combo");

    let orders2: Vec<i32> = {
        let mut stmt = conn
            .prepare("SELECT priority_order FROM combo_targets WHERE combo_id = ?1 ORDER BY priority_order")
            .unwrap();
        let rows = stmt
            .query_map(rusqlite::params![combo_id], |r| r.get::<_, i32>(0))
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    };
    assert_eq!(orders2, vec![0, 1, 2, 3]);
}
