//! Unit tests for the `models` module, decomposed into cohesive domains.

use super::*;
use crate::error::CoreError;
use crate::ids::{ModelId, ModelRowId, ProviderId};
use rusqlite::Connection;
use std::time::Duration;

/// Set up an in-memory DB with the same DDL the production migrations
/// produce for the `models` table (plus a row in `providers` to satisfy
/// the FK — we only need the parent row to exist).
pub(crate) fn fresh_db() -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory");
    conn.execute_batch(
        "CREATE TABLE providers (
                 id            TEXT PRIMARY KEY,
                 display_name  TEXT NOT NULL,
                 base_url      TEXT NOT NULL,
                 auth_kind     TEXT NOT NULL,
                 health_status TEXT NOT NULL DEFAULT 'healthy',
                 active        INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
                 created_at    TEXT NOT NULL DEFAULT (datetime('now')),
                 notif_keyword_only INTEGER NOT NULL DEFAULT 0 CHECK (notif_keyword_only IN (0, 1)),
                 CHECK (health_status IN ('healthy', 'degraded', 'unhealthy'))
             );
             CREATE TABLE models (
                 id                     INTEGER PRIMARY KEY AUTOINCREMENT,
                 provider_id            TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
                 model_id               TEXT NOT NULL,
                 display_name           TEXT,
                 target_format          TEXT NOT NULL,
                 discovered_at          TEXT NOT NULL DEFAULT (datetime('now')),
                 expires_at             TEXT,
                 timeout_overrides_json TEXT,
                 active                 INTEGER NOT NULL DEFAULT 1
                                          CHECK (active IN (0, 1)),
                 last_test_status       INTEGER,
                 last_test_at           TEXT,
                 custom                 INTEGER NOT NULL DEFAULT 0
                                          CHECK (custom IN (0, 1)),
                 context_length         INTEGER,
                 max_output_tokens      INTEGER,
                 capabilities_json      TEXT,
                 family                 TEXT,
                 model_type             TEXT NOT NULL DEFAULT 'chat',
                 input_modalities_json  TEXT,
                 output_modalities_json TEXT,
                 model_id_normalized    TEXT,
                 manually_disabled_at   TEXT,
                 UNIQUE(provider_id, model_id),
                 CHECK (target_format IN ('openai', 'anthropic', 'gemini', 'responses'))
             );
             INSERT INTO providers (id, display_name, base_url, auth_kind)
             VALUES ('provA', 'Provider A', 'https://example.test', 'none');",
    )
    .expect("schema");
    conn
}

pub(crate) fn discovered(id: &str, fmt: TargetFormat) -> DiscoveredModel {
    DiscoveredModel {
        model_id: ModelId::new(id),
        display_name: Some(format!("Display {id}")),
        target_format: fmt,
        context_length: None,
        max_output_tokens: None,
        input_modalities: None,
        output_modalities: None,
        model_type: None,
        family: None,
        capabilities: None,
    }
}

pub(crate) fn minimal(id: &str) -> DiscoveredModel {
    DiscoveredModel {
        model_id: ModelId::new(id),
        display_name: Some(id.to_string()),
        target_format: TargetFormat::Openai,
        context_length: None,
        max_output_tokens: None,
        input_modalities: None,
        output_modalities: None,
        model_type: None,
        family: None,
        capabilities: None,
    }
}

pub(crate) fn add_provider(conn: &Connection, id: &str) {
    conn.execute(
        "INSERT INTO providers (id, display_name, base_url, auth_kind)
         VALUES (?1, ?1, 'https://example.test', 'none')",
        [id],
    )
    .expect("add provider");
}

pub(crate) fn manually_disabled_epoch(
    conn: &Connection,
    row_id: ModelRowId,
) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT CAST(strftime('%s', manually_disabled_at) AS INTEGER) FROM models WHERE id = ?1",
        [row_id.0],
        |r| r.get::<_, Option<i64>>(0),
    )
}

pub(crate) fn now_epoch(conn: &Connection) -> i64 {
    conn.query_row("SELECT CAST(strftime('%s', 'now') AS INTEGER)", [], |r| {
        r.get::<_, i64>(0)
    })
    .expect("now_epoch")
}

mod auto_activation;
mod cascade;
mod custom;
mod lifecycle;
mod reconnect;
mod upsert;
