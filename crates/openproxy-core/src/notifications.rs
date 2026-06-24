//! Notifications tray: surfaces discovery + system events to dashboard users.
//!
//! ## Design
//!
//! - **Persistence**: `notifications` table (migration 000036). Each row is
//!   one notification. Rows are never updated except for `read_at`/`archived_at`.
//! - **Push**: a process-global `tokio::sync::broadcast::Sender<NotificationEvent>`
//!   (capacity 256). The WS handler (F2) subscribes and pushes to clients.
//! - **Generation**: notification rows are inserted inside the `upsert_many`
//!   transaction (for model_new/model_gone) and inside `apply_auto_activation`
//!   (for model_auto_activated), so they commit atomically with the model
//!   changes. System notifications are inserted at the call site of the error.
//! - **De-duplication**: the `idx_notifications_dedup` unique index on
//!   `(kind, dedup_key, date(created_at))` collapses duplicates within 24h.
//!   The INSERT uses `INSERT OR IGNORE` so duplicates are silently dropped.
//!
//! ## Adding a new notification kind
//!
//! 1. Add the kind string to the CHECK constraint in migration 000037 (a new
//!    migration — schema migrations are append-only).
//! 2. Add a constant `pub const KIND_FOO: &str = "foo";` below.
//! 3. Add a payload struct `pub struct FooPayload { ... }` and implement
//!    `serde::Serialize` for it.
//! 4. Add a helper `pub fn record_foo(conn, payload) -> Result<()>`.
//! 5. Call the helper from the relevant code path.
//! 6. The frontend handles the new kind in the notifications view.

use anyhow::Result;
use once_cell::sync::OnceCell;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

pub const KIND_MODEL_NEW: &str = "model_new";
pub const KIND_MODEL_GONE: &str = "model_gone";
pub const KIND_MODEL_AUTO_ACTIVATED: &str = "model_auto_activated";
pub const KIND_SYSTEM: &str = "system";

pub const BROADCAST_CAPACITY: usize = 256;

/// Process-global broadcast channel for real-time push to WS clients.
/// Subscribed by `stream_usage_rows` in handlers/admin.rs (see F2).
pub static NOTIF_TX: OnceCell<broadcast::Sender<NotificationEvent>> = OnceCell::new();

/// Initialize the broadcast channel. Called once at server startup from
/// state.rs. Idempotent — subsequent calls are no-ops and return the
/// already-installed sender.
pub fn init_broadcast() -> &'static broadcast::Sender<NotificationEvent> {
    NOTIF_TX.get_or_init(|| {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        tx
    })
}

/// Get the sender if initialized. Returns `None` if `init_broadcast` hasn't
/// been called yet (e.g. in tests that don't boot the full AppState).
pub fn try_get_tx() -> Option<&'static broadcast::Sender<NotificationEvent>> {
    NOTIF_TX.get()
}

/// Real-time event pushed to WS clients. The `id` is the DB row id, which
/// the client can use to fetch the full row if needed (rarely necessary —
/// the payload has everything the tray needs to render).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NotificationEvent {
    pub id: i64,
    pub kind: String,
    pub payload: serde_json::Value,
    pub created_at: String,
}

// Per-kind payload structs. These are the contract between Rust and the
// frontend — changes here MUST be reflected in the TypeScript types.

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelNewPayload {
    pub provider_id: String,
    pub model_id: String,
    pub display_name: Option<String>,
    pub target_format: String,
    pub context_length: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelGonePayload {
    pub provider_id: String,
    pub model_id: String,
    /// The display_name the model had when it was deleted. May be `None` if
    /// we couldn't read it before the DELETE.
    pub display_name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelAutoActivatedPayload {
    pub provider_id: String,
    pub model_id: String,
    pub display_name: Option<String>,
    /// The keyword that matched (from `providers.auto_activate_keyword`).
    /// `None` means "provider had no keyword, all new models auto-activated".
    pub matched_keyword: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SystemPayload {
    /// Stable machine-readable code, e.g. `"discovery_failed"`,
    /// `"oauth_expired"`, `"circuit_opened"`. Frontend can use this for
    /// icon/color if desired.
    pub code: String,
    /// Human-readable message.
    pub message: String,
    /// Optional provider_id if the system event is provider-scoped.
    pub provider_id: Option<String>,
    /// Optional free-form details (e.g. the error string).
    pub details: Option<serde_json::Value>,
}

// ---------- DB operations ----------

/// A notification row, as returned by [`list`] and (conceptually) [`get`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NotificationRow {
    pub id: i64,
    pub kind: String,
    pub payload: serde_json::Value,
    pub read_at: Option<String>,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub dedup_key: Option<String>,
    pub provider_id: Option<String>,
}

/// Insert a notification row. Uses `INSERT OR IGNORE` so the dedup unique
/// index silently drops duplicates within the same UTC day.
///
/// Returns the row id (`Some`) if a new row was inserted, or `None` if the
/// insert was ignored due to dedup *and* no matching existing row could be
/// located. When the insert is deduped, the function attempts to look up
/// the existing row's id and returns `Some(existing_id)` so callers can
/// still broadcast (the broadcast is idempotent on the client side because
/// the dashboard dedupes by id).
pub fn insert(
    conn: &Connection,
    kind: &str,
    payload: &serde_json::Value,
    dedup_key: Option<&str>,
    provider_id: Option<&str>,
) -> Result<Option<i64>> {
    let payload_str = serde_json::to_string(payload)?;
    let changed = conn.execute(
        "INSERT OR IGNORE INTO notifications (kind, payload_json, dedup_key, provider_id)
         VALUES (?1, ?2, ?3, ?4)",
        params![kind, payload_str, dedup_key, provider_id],
    )?;
    if changed == 0 {
        // Dedup hit — find the existing row id. We match on the same
        // triple the unique index uses so we resolve to exactly the row
        // that blocked the insert.
        let existing: Option<i64> = if let Some(dk) = dedup_key {
            conn.query_row(
                "SELECT id FROM notifications
                 WHERE kind = ?1 AND dedup_key = ?2 AND date(created_at) = date('now')
                 LIMIT 1",
                params![kind, dk],
                |row| row.get(0),
            )
            .optional()?
        } else {
            None
        };
        Ok(existing)
    } else {
        Ok(Some(conn.last_insert_rowid()))
    }
}

/// Same as [`insert`] but also broadcasts the event to WS clients if a new
/// row was inserted (or an existing dedup row was found). This is the
/// primary entry point from non-transactional code paths (e.g. system
/// notifications from the scheduler).
pub fn insert_and_broadcast(
    conn: &Connection,
    kind: &str,
    payload: &serde_json::Value,
    dedup_key: Option<&str>,
    provider_id: Option<&str>,
) -> Result<Option<i64>> {
    let id = insert(conn, kind, payload, dedup_key, provider_id)?;
    if let Some(id) = id {
        broadcast_one(conn, id, kind, payload)?;
    }
    Ok(id)
}

/// Broadcast an already-inserted notification to WS clients. Used when the
/// insert happened inside a transaction (e.g. `upsert_many`) and we can't
/// broadcast from within the tx (the row isn't visible to other connections
/// until commit). Called AFTER the transaction commits.
///
/// Failures here are logged at most once and never bubble — broadcast send
/// errors (no subscribers) are expected during cold start and unit tests.
pub fn broadcast_one(
    conn: &Connection,
    id: i64,
    kind: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    let created_at: String = conn.query_row(
        "SELECT created_at FROM notifications WHERE id = ?1",
        params![id],
        |row| row.get(0),
    )?;
    if let Some(tx) = try_get_tx() {
        // `broadcast::send` returns Err when there are no active
        // receivers; that's not a real error, so we swallow it.
        let _ = tx.send(NotificationEvent {
            id,
            kind: kind.to_string(),
            payload: payload.clone(),
            created_at,
        });
    }
    Ok(())
}

/// Convenience: insert + broadcast for system notifications. This is the
/// primary entry point for "scheduler failed", "oauth expired", etc.
///
/// The dedup key is the `code` itself, so repeat identical codes within
/// 24h collapse into a single row.
pub fn record_system(
    conn: &Connection,
    code: &str,
    message: &str,
    provider_id: Option<&str>,
    details: Option<serde_json::Value>,
) -> Result<Option<i64>> {
    let payload = serde_json::json!({
        "code": code,
        "message": message,
        "provider_id": provider_id,
        "details": details,
    });
    insert_and_broadcast(conn, KIND_SYSTEM, &payload, Some(code), provider_id)
}

// ---------- Query API for the dashboard ----------

/// List notifications, most recent first (by descending id).
///
/// - `unread_only`: if `true`, filter to `read_at IS NULL`.
/// - `limit`: max rows to return, clamped to `[1, 200]`.
/// - `before_id`: for cursor pagination — only return rows with `id < before_id`.
///
/// Archived rows (`archived_at IS NOT NULL`) are always excluded; they are
/// audit-only and hidden from the tray UI.
pub fn list(
    conn: &Connection,
    unread_only: bool,
    limit: i64,
    before_id: Option<i64>,
) -> Result<Vec<NotificationRow>> {
    let limit = limit.clamp(1, 200);
    // We always bind both named params (using COALESCE so a NULL
    // `:before` degenerates to "no upper bound"). This avoids the
    // rusqlite "Invalid parameter name" error that fires when the SQL
    // doesn't reference a param we tried to bind.
    let sql = format!(
        "SELECT id, kind, payload_json, read_at, archived_at, created_at, dedup_key, provider_id
         FROM notifications
         WHERE archived_at IS NULL{unread}
           AND id < COALESCE(:before, 9223372036854775807)
         ORDER BY id DESC LIMIT :limit",
        unread = if unread_only { " AND read_at IS NULL" } else { "" }
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        &[
            (":before", &before_id as &dyn rusqlite::ToSql),
            (":limit", &limit as &dyn rusqlite::ToSql),
        ],
        |row| {
            let payload_str: String = row.get(2)?;
            let payload: serde_json::Value =
                serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Null);
            Ok(NotificationRow {
                id: row.get(0)?,
                kind: row.get(1)?,
                payload,
                read_at: row.get(3)?,
                archived_at: row.get(4)?,
                created_at: row.get(5)?,
                dedup_key: row.get(6)?,
                provider_id: row.get(7)?,
            })
        },
    )?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Count unread, non-archived notifications. For the sidebar badge.
pub fn unread_count(conn: &Connection) -> Result<i64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM notifications
         WHERE read_at IS NULL AND archived_at IS NULL",
        [],
        |row| row.get(0),
    )?;
    Ok(count)
}

/// Mark a single notification as read (sets `read_at` to now). Idempotent.
pub fn mark_read(conn: &Connection, id: i64) -> Result<()> {
    conn.execute(
        "UPDATE notifications SET read_at = datetime('now') WHERE id = ?1 AND read_at IS NULL",
        params![id],
    )?;
    Ok(())
}

/// Mark all unread, non-archived notifications as read. Returns the number
/// of rows updated.
pub fn mark_all_read(conn: &Connection) -> Result<usize> {
    let changed = conn.execute(
        "UPDATE notifications SET read_at = datetime('now')
         WHERE read_at IS NULL AND archived_at IS NULL",
        [],
    )?;
    Ok(changed)
}

/// Archive a single notification (sets `archived_at` to now). The row is
/// preserved for audit. Idempotent.
pub fn archive(conn: &Connection, id: i64) -> Result<()> {
    conn.execute(
        "UPDATE notifications SET archived_at = datetime('now')
         WHERE id = ?1 AND archived_at IS NULL",
        params![id],
    )?;
    Ok(())
}

/// Permanently delete a notification. Only allowed for `kind = 'system'`
/// or rows older than 30 days (to preserve `model_*` audit history).
///
/// Returns `Ok(true)` if a row was deleted, `Ok(false)` if the row was
/// not eligible (or didn't exist). The HTTP handler maps `Ok(false)` to
/// HTTP 403 so the client knows the delete was refused, not silently
/// dropped.
pub fn delete(conn: &Connection, id: i64) -> Result<bool> {
    let changed = conn.execute(
        "DELETE FROM notifications
         WHERE id = ?1 AND (
             kind = 'system'
             OR created_at < datetime('now', '-30 days')
         )",
        params![id],
    )?;
    Ok(changed > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;

    fn fresh_db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        migrations::run(&mut conn).unwrap();
        conn
    }

    #[test]
    fn insert_and_dedup() {
        let conn = fresh_db();
        let payload = serde_json::json!({"provider_id":"p1","model_id":"m1"});
        let id1 = insert(&conn, KIND_MODEL_NEW, &payload, Some("p1:m1"), Some("p1")).unwrap();
        let id2 = insert(&conn, KIND_MODEL_NEW, &payload, Some("p1:m1"), Some("p1")).unwrap();
        assert!(id1.is_some());
        // Second insert within same day is deduped — returns the existing id.
        assert_eq!(id1, id2);
    }

    #[test]
    fn unread_count_works() {
        let conn = fresh_db();
        assert_eq!(unread_count(&conn).unwrap(), 0);
        insert(
            &conn,
            KIND_MODEL_NEW,
            &serde_json::json!({}),
            Some("p1:m1"),
            Some("p1"),
        )
        .unwrap();
        insert(
            &conn,
            KIND_MODEL_NEW,
            &serde_json::json!({}),
            Some("p1:m2"),
            Some("p1"),
        )
        .unwrap();
        assert_eq!(unread_count(&conn).unwrap(), 2);
        let id = list(&conn, true, 10, None).unwrap()[0].id;
        mark_read(&conn, id).unwrap();
        assert_eq!(unread_count(&conn).unwrap(), 1);
    }

    #[test]
    fn mark_all_read_works() {
        let conn = fresh_db();
        insert(
            &conn,
            KIND_MODEL_NEW,
            &serde_json::json!({}),
            Some("p1:m1"),
            Some("p1"),
        )
        .unwrap();
        insert(
            &conn,
            KIND_MODEL_NEW,
            &serde_json::json!({}),
            Some("p1:m2"),
            Some("p1"),
        )
        .unwrap();
        assert_eq!(mark_all_read(&conn).unwrap(), 2);
        assert_eq!(unread_count(&conn).unwrap(), 0);
    }

    #[test]
    fn delete_system_allowed_model_not() {
        let conn = fresh_db();
        let sys_id = insert(
            &conn,
            KIND_SYSTEM,
            &serde_json::json!({"code":"x","message":"y"}),
            Some("x"),
            None,
        )
        .unwrap()
        .unwrap();
        let model_id = insert(
            &conn,
            KIND_MODEL_NEW,
            &serde_json::json!({}),
            Some("p1:m1"),
            Some("p1"),
        )
        .unwrap()
        .unwrap();
        // System can be deleted immediately.
        assert!(delete(&conn, sys_id).unwrap());
        // Model_new cannot (within 30 days).
        assert!(!delete(&conn, model_id).unwrap());
        // Verify
        assert!(
            list(&conn, false, 10, None)
                .unwrap()
                .iter()
                .all(|r| r.id != sys_id)
        );
        assert!(
            list(&conn, false, 10, None)
                .unwrap()
                .iter()
                .any(|r| r.id == model_id)
        );
    }

    #[test]
    fn archive_hides_from_list() {
        let conn = fresh_db();
        let id = insert(
            &conn,
            KIND_MODEL_NEW,
            &serde_json::json!({}),
            Some("p1:m1"),
            Some("p1"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(list(&conn, false, 10, None).unwrap().len(), 1);
        archive(&conn, id).unwrap();
        assert_eq!(list(&conn, false, 10, None).unwrap().len(), 0);
    }

    #[test]
    fn list_pagination_with_before_id() {
        let conn = fresh_db();
        for i in 0..5 {
            insert(
                &conn,
                KIND_MODEL_NEW,
                &serde_json::json!({"i": i}),
                Some(&format!("p1:m{}", i)),
                Some("p1"),
            )
            .unwrap();
        }
        let all = list(&conn, false, 100, None).unwrap();
        assert_eq!(all.len(), 5);
        // ids are descending
        let mid_id = all[2].id;
        let before = list(&conn, false, 100, Some(mid_id)).unwrap();
        assert!(
            before.iter().all(|r| r.id < mid_id),
            "before_id should exclude id >= mid_id"
        );
        assert_eq!(before.len(), 2);
    }

    #[test]
    fn record_system_dedupes_by_code() {
        let conn = fresh_db();
        let id1 =
            record_system(&conn, "discovery_failed", "boom", Some("p1"), None).unwrap();
        let id2 =
            record_system(&conn, "discovery_failed", "boom-again", Some("p1"), None).unwrap();
        // Same code within 24h collapses to the same row.
        assert_eq!(id1, id2);
        assert_eq!(unread_count(&conn).unwrap(), 1);
    }
}
