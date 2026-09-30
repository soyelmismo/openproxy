//! Notifications tray: surfaces discovery + system events to dashboard users.
//!
//! ## Design
//!
//! - **Persistence**: `notifications` table (migration 000036). Rows are never
//!   updated except for `read_at`/`archived_at`.
//! - **Push**: process-global `broadcast::Sender<NotificationEvent>` (capacity
//!   [`BROADCAST_CAPACITY`]). The WS handler subscribes and pushes to clients.
//! - **Generation**: rows commit atomically with the model changes they
//!   describe (inside `upsert_many` / `apply_auto_activation`), so the
//!   broadcast happens after the transaction commits.
//! - **De-duplication**: the `idx_notifications_dedup` unique index on
//!   `(kind, dedup_key, date(created_at))` collapses duplicates within 24h via
//!   `INSERT OR IGNORE`.
//!
//! Adding a kind: append a migration extending the CHECK constraint, add the
//! `KIND_*` const and its `Serialize` payload, add a `record_*` helper, and
//! handle the kind in the notifications view.

use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::broadcast;

pub const KIND_MODEL_NEW: &str = "model_new";
pub const KIND_MODEL_GONE: &str = "model_gone";
pub const KIND_MODEL_AUTO_ACTIVATED: &str = "model_auto_activated";
pub const KIND_SYSTEM: &str = "system";

pub const BROADCAST_CAPACITY: usize = 256;

// System notification codes. `code` is the stable machine-readable identifier
// the frontend maps to an icon/color/body template, and it also selects the
// dedup semantics: `record_system` dedupes by `code` alone (one row per code
// per 24h, for provider-wide events), while per-entity events pass a
// `dedup_key` like `circuit_open:{account_id}` to `insert_and_broadcast` so
// each entity gets its own row and one entity flapping within 24h collapses.

/// Discovery tick failed for a provider (network down, upstream 5xx,
/// bad key). Emitted by `discovery_scheduler`. Dedup: per-code (one
/// row per 24h per provider, via `record_system`).
pub const CODE_DISCOVERY_FAILED: &str = "discovery_failed";

/// An account's API key failed to decrypt (wrong master key, corrupt
/// ciphertext). Emitted by `discovery_scheduler`. Dedup: per-code.
pub const CODE_ACCOUNT_KEY_DECRYPT_FAILED: &str = "account_key_decrypt_failed";

/// A per-account circuit breaker transitioned from closed (Healthy) to
/// open (Unhealthy) after the failure threshold was reached. Emitted
/// by the pipeline's `execute_single` post-dispatch path. Dedup:
/// per-account (`circuit_open:{account_id}`).
pub const CODE_CIRCUIT_OPEN: &str = "circuit_open";

/// An OAuth token refresh failed repeatedly, or an OAuth-protected
/// request returned 401 and the token couldn't be refreshed. Emitted
/// by `oauth::start_refresh_scheduler` and the pipeline's proactive
/// refresh path. Dedup: per-account (`oauth_expired:{account_id}`).
pub const CODE_OAUTH_EXPIRED: &str = "oauth_expired";

/// An account's API key is being rejected by the upstream (401/403).
/// Emitted by the pipeline's `dispatch_upstream` 4xx detection path.
/// Dedup: per-account (`account_invalid:{account_id}`).
pub const CODE_ACCOUNT_INVALID: &str = "account_invalid";

/// An account's remaining quota is below the low-water threshold
/// (default 10% of the limit). Emitted by the
/// `refresh_account_quota` admin handler after a successful fetch.
/// Dedup: per-account (`quota_low:{account_id}`).
pub const CODE_QUOTA_LOW: &str = "quota_low";

/// An account's quota fetch attempt failed.
/// Dedup: per-account (`quota_fetch_failed:{account_id}`).
pub const CODE_QUOTA_FETCH_FAILED: &str = "quota_fetch_failed";

/// A proxy assigned to a provider/account failed connection or was rotated.
/// Dedup: per-provider (`proxy_failed:{provider_id}`).
pub const CODE_PROXY_FAILED: &str = "proxy_failed";

/// Process-global broadcast channel for real-time push to WS clients.
/// Subscribed by `stream_usage_rows` in handlers/admin.rs (see F2).
pub static NOTIF_TX: OnceLock<broadcast::Sender<NotificationEvent>> = OnceLock::new();

/// Process-global master switch. When `false` every insert path early-returns
/// without touching the DB or the broadcast channel. Hydrated at boot from the
/// `notifications_enabled` key in `app_config` and flipped at runtime by
/// `PUT /admin/api/config/notifications-enabled`.
static NOTIFICATIONS_ENABLED: AtomicBool =
    AtomicBool::new(openproxy_db::app_config::NOTIFICATIONS_ENABLED_DEFAULT);

/// Read the current global notifications flag.
pub fn is_enabled() -> bool {
    NOTIFICATIONS_ENABLED.load(Ordering::Relaxed)
}

/// Replace the live global notifications flag. Called by the admin
/// PUT endpoint after the DB UPSERT and by the boot hydration path.
pub fn set_enabled(enabled: bool) {
    NOTIFICATIONS_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Initialize the broadcast channel. Called once at server startup from
/// state.rs. Idempotent — subsequent calls are no-ops and return the
/// already-installed sender.
pub fn init_broadcast() -> &'static broadcast::Sender<NotificationEvent> {
    NOTIF_TX.get_or_init(|| {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        let tx_clone = broadcast::Sender::clone(&tx);
        let _ =
            openproxy_types::notifications::NOTIFICATION_PUBLISHER.set(Box::new(move |event| {
                let _ = tx_clone.send(event);
            }));
        tx
    })
}

/// Get the sender if initialized. Returns `None` if `init_broadcast` hasn't
/// been called yet (e.g. in tests that don't boot the full AppState).
pub fn try_get_tx() -> Option<&'static broadcast::Sender<NotificationEvent>> {
    NOTIF_TX.get()
}

pub use openproxy_types::NotificationEvent;

// Per-kind payload structs. These are the contract between Rust and the
// frontend: changes here belong in the TypeScript types too.

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
    /// The display_name the model had when it was deleted. `None` when it
    /// could not be read before the DELETE.
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

// DB operations, re-exported from openproxy-db.

pub use openproxy_db::notifications::{
    NotificationRow, RETENTION_DAYS, RETENTION_OFFSET, archive, archive_all, delete, insert,
    insert_many, list, mark_all_read, mark_read, prune, unread_count,
};

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
    // Disabled: skip insert and broadcast. Callers treat `Ok(None)` as
    // "nothing emitted".
    if !is_enabled() {
        return Ok(None);
    }
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
/// Dispatches broadcast asynchronously if a Tokio runtime is present, releasing
/// the caller and its database connection lock before channel transmission (AGENTS.md §4.3).
///
/// Never bubbles: `send` errors from an empty receiver set are expected during
/// cold start and unit tests.
pub fn broadcast_one(
    conn: &Connection,
    id: i64,
    kind: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    // W1 global gate: nothing to broadcast when notifications are off.
    if !is_enabled() {
        return Ok(());
    }
    let created_at = openproxy_db::notifications::get_created_at(conn, id)?
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    broadcast_event(NotificationEvent {
        id,
        kind: kind.to_string(),
        payload: payload.to_owned(),
        created_at,
    });
    Ok(())
}

/// Broadcast a [`NotificationEvent`] to subscribers, offloading transmission to the
/// background runtime when available to decouple channel delivery from database locks.
pub fn broadcast_event(event: NotificationEvent) {
    if let Some(tx) = try_get_tx() {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = tx.send(event);
            });
        } else {
            let _ = tx.send(event);
        }
    }
}

/// Insert + broadcast for system notifications. The dedup key is the `code`,
/// so repeats within 24h collapse into a single row.
pub fn record_system(
    conn: &Connection,
    code: &str,
    message: &str,
    provider_id: Option<&str>,
    details: Option<&serde_json::Value>,
) -> Result<Option<i64>> {
    // Disabled: skip payload construction too.
    if !is_enabled() {
        return Ok(None);
    }
    let payload = serde_json::json!({
        "code": code,
        "message": message,
        "provider_id": provider_id,
        "details": details,
    });
    insert_and_broadcast(conn, KIND_SYSTEM, &payload, Some(code), provider_id)
}

/// Gate for the batch insert path (`models::sync::upsert_many`). `false` means
/// the caller skips the INSERT entirely: no rows, no broadcast.
pub fn insert_many_enabled() -> bool {
    is_enabled()
}

/// [`insert_many`] plus the global gate: returns an empty result when
/// notifications are disabled. Transactional callers use this so the gate does
/// not have to live in the DAO-level `insert_many`.
pub fn insert_many_gated(
    conn: &Connection,
    kind: &str,
    rows: &[(serde_json::Value, Option<String>, Option<String>)],
) -> Result<Vec<(i64, serde_json::Value)>> {
    if !is_enabled() {
        return Ok(Vec::new());
    }
    let res = openproxy_db::notifications::insert_many(conn, kind, rows)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn fresh_db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        openproxy_db::migrations::run(&mut conn).unwrap();
        conn
    }

    #[test]
    fn insert_and_dedup() {
        let conn = fresh_db();
        let payload = serde_json::json!({"provider_id":"p1","model_id":"m1"});
        let id1 = insert(&conn, KIND_MODEL_NEW, &payload, Some("p1:m1"), Some("p1")).unwrap();
        let id2 = insert(&conn, KIND_MODEL_NEW, &payload, Some("p1:m1"), Some("p1")).unwrap();
        assert!(id1.is_some());
        // Segundo insert del mismo día se deduplica y devuelve el id existente.
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
        // system rows are immediately deletable, model_new rows are kept
        // for RETENTION_DAYS
        assert!(delete(&conn, sys_id).unwrap());
        assert!(!delete(&conn, model_id).unwrap());
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
    fn archive_all_hides_all_from_list() {
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
            KIND_SYSTEM,
            &serde_json::json!({}),
            Some("p1:m2"),
            Some("p1"),
        )
        .unwrap();
        assert_eq!(list(&conn, false, 10, None).unwrap().len(), 2);
        let count = archive_all(&conn).unwrap();
        assert_eq!(count, 2);
        assert_eq!(list(&conn, false, 10, None).unwrap().len(), 0);
        assert_eq!(unread_count(&conn).unwrap(), 0);
    }

    #[test]
    fn list_pagination_with_before_id() {
        let conn = fresh_db();
        for i in 0..5 {
            insert(
                &conn,
                KIND_MODEL_NEW,
                &serde_json::json!({"i": i}),
                Some(&format!("p1:m{i}")),
                Some("p1"),
            )
            .unwrap();
        }
        let all = list(&conn, false, 100, None).unwrap();
        assert_eq!(all.len(), 5);
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
        let id1 = record_system(&conn, "discovery_failed", "boom", Some("p1"), None).unwrap();
        let id2 = record_system(&conn, "discovery_failed", "boom-again", Some("p1"), None).unwrap();
        // Same code within 24h collapses to the same row.
        assert_eq!(id1, id2);
        assert_eq!(unread_count(&conn).unwrap(), 1);
    }

    // `unread_count` MUST filter `archived_at IS NULL` alongside `read_at IS
    // NULL`: archive does not touch `read_at`, so an archived-but-unread row
    // would keep the badge from dropping after a dismiss.
    #[test]
    fn archived_rows_excluded_from_unread_count() {
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
        assert_eq!(unread_count(&conn).unwrap(), 1);
        archive(&conn, id).unwrap();
        assert_eq!(unread_count(&conn).unwrap(), 0);
        let row = list(&conn, false, 10, None).unwrap();
        assert!(row.is_empty(), "archived row should be hidden from list");
    }

    // `mark_all_read` MUST filter `archived_at IS NULL`: archived rows are
    // immutable apart from `archived_at`, and the returned count feeds the
    // client's "N rows updated" readout.
    #[test]
    fn mark_all_read_skips_archived_rows() {
        let conn = fresh_db();
        let id_active = insert(
            &conn,
            KIND_MODEL_NEW,
            &serde_json::json!({}),
            Some("p1:active"),
            Some("p1"),
        )
        .unwrap()
        .unwrap();
        let id_archived = insert(
            &conn,
            KIND_MODEL_NEW,
            &serde_json::json!({}),
            Some("p1:archived"),
            Some("p1"),
        )
        .unwrap()
        .unwrap();
        archive(&conn, id_archived).unwrap();
        let changed = mark_all_read(&conn).unwrap();
        assert_eq!(changed, 1, "mark_all_read should skip archived rows");
        assert_eq!(unread_count(&conn).unwrap(), 0);
        // raw columns: list() hides archived rows
        let active_read_at: Option<String> = conn
            .query_row(
                "SELECT read_at FROM notifications WHERE id = ?1",
                params![id_active],
                |row| row.get(0),
            )
            .unwrap();
        let archived_read_at: Option<String> = conn
            .query_row(
                "SELECT read_at FROM notifications WHERE id = ?1",
                params![id_archived],
                |row| row.get(0),
            )
            .unwrap();
        assert!(active_read_at.is_some(), "active row should be marked read");
        assert!(
            archived_read_at.is_none(),
            "archived row should NOT be marked read by mark_all_read"
        );
    }

    #[test]
    fn test_insert_many_large_batch() {
        let conn = fresh_db();
        let count = 350;
        let mut rows = Vec::with_capacity(count);
        for i in 0..count {
            rows.push((
                serde_json::json!({"item": i}),
                Some(format!("dedup_{i}")),
                Some("test_provider".to_string()),
            ));
        }

        let inserted = insert_many(&conn, KIND_MODEL_NEW, &rows).unwrap();
        assert_eq!(inserted.len(), count);

        // the dedup index returns the same ids on re-insert
        let reinserted = insert_many(&conn, KIND_MODEL_NEW, &rows).unwrap();
        assert_eq!(reinserted.len(), count);
        assert_eq!(inserted, reinserted);
    }

    #[tokio::test]
    async fn test_broadcast_one_releases_lock_without_blocking() {
        init_broadcast();
        let conn = fresh_db();
        let payload = serde_json::json!({"test": "data"});
        let id = insert(&conn, KIND_MODEL_NEW, &payload, Some("p_test:m_test"), Some("p_test"))
            .unwrap()
            .unwrap();
        let res = broadcast_one(&conn, id, KIND_MODEL_NEW, &payload);
        assert!(res.is_ok());
    }
}
