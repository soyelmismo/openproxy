//! Notification persistence and DAO operations.

use openproxy_types::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// A notification row, as returned by [`list`].
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

crate::def_table_select!(
    notification_select,
    "notifications",
    "id, kind, payload_json, read_at, archived_at, created_at, dedup_key, provider_id"
);

crate::def_table_select!(notification_id_select, "notifications", "id");

crate::def_table_select!(notification_dedup_select, "notifications", "id, dedup_key");

crate::def_table_select!(
    notification_created_at_select,
    "notifications",
    "created_at"
);

impl crate::crud::FromRow for NotificationRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        crate::map_row_struct!(row, NotificationRow {
            id: 0,
            kind: 1,
            payload: @json_or_default(2),
            read_at: 3,
            archived_at: 4,
            created_at: 5,
            dedup_key: 6,
            provider_id: 7,
        })
    }
}

/// Insert a notification row. Uses `INSERT OR IGNORE` so the dedup unique
/// index silently drops duplicates within the same UTC day.
///
/// Returns the row id (`Some`) if a new row was inserted, or `None` if the
/// insert was ignored due to dedup *and* no matching existing row could be
/// located. When the insert is deduped, the function attempts to look up
/// the existing row's id and returns `Some(existing_id)`.
pub fn insert(
    conn: &Connection,
    kind: &str,
    payload: &serde_json::Value,
    dedup_key: Option<&str>,
    provider_id: Option<&str>,
) -> Result<Option<i64>> {
    let payload_str = serde_json::to_string(payload).map_err(|e| {
        openproxy_types::error::CoreError::Validation(format!(
            "serialize notification payload: {e}"
        ))
    })?;
    let changed = conn
        .execute(
            "INSERT OR IGNORE INTO notifications (kind, payload_json, dedup_key, provider_id)
             VALUES (?1, ?2, ?3, ?4)",
            params![kind, payload_str, dedup_key, provider_id],
        )
        .map_err(crate::error::map_db_error_ctx("insert notification"))?;
    if changed == 0 {
        dedup_notification_id(conn, kind, dedup_key)
    } else {
        Ok(Some(conn.last_insert_rowid()))
    }
}

fn dedup_notification_id(
    conn: &Connection,
    kind: &str,
    dedup_key: Option<&str>,
) -> Result<Option<i64>> {
    let Some(dk) = dedup_key else {
        return Ok(None);
    };
    conn.query_row(
        notification_id_select!(
            "WHERE kind = ?1 AND dedup_key = ?2 AND date(created_at) = date('now') \
             LIMIT 1"
        ),
        params![kind, dk],
        |row| row.get(0),
    )
    .optional()
    .map_err(crate::error::map_db_error_ctx(
        "query dedup notification id",
    ))
}

fn build_notification_chunk_params(
    kind: &str,
    chunk: &[(serde_json::Value, Option<String>, Option<String>)],
) -> Result<Vec<rusqlite::types::Value>> {
    let mut params = Vec::with_capacity(chunk.len() * 4);
    for row in chunk {
        params.push(kind.to_owned().into());
        let payload_str = serde_json::to_string(&row.0).map_err(|e| {
            openproxy_types::error::CoreError::Validation(format!(
                "serialize notification payload: {e}"
            ))
        })?;
        params.push(payload_str.into());
        params.push(
            row.1
                .as_ref()
                .map_or(rusqlite::types::Value::Null, |k| k.to_owned().into()),
        );
        params.push(
            row.2
                .as_ref()
                .map_or(rusqlite::types::Value::Null, |p| p.to_owned().into()),
        );
    }
    Ok(params)
}

fn collect_inserted_ids(
    mut rows: rusqlite::Rows<'_>,
) -> Result<(std::collections::HashMap<String, i64>, Vec<i64>)> {
    let mut inserted_ids_by_dedup = std::collections::HashMap::new();
    let mut inserted_ids_no_dedup = Vec::new();

    while let Some(r) = rows.next().map_err(crate::error::map_db_error)? {
        let id: i64 = r.get(0).map_err(crate::error::map_db_error)?;
        let dedup_key: Option<String> = r.get(1).map_err(crate::error::map_db_error)?;
        if let Some(dk) = dedup_key {
            inserted_ids_by_dedup.insert(dk, id);
        } else {
            inserted_ids_no_dedup.push(id);
        }
    }
    Ok((inserted_ids_by_dedup, inserted_ids_no_dedup))
}

fn execute_insert_chunk(
    conn: &Connection,
    kind: &str,
    chunk: &[(serde_json::Value, Option<String>, Option<String>)],
) -> Result<(std::collections::HashMap<String, i64>, Vec<i64>)> {
    let sql = crate::batch::build_insert_sql(
        "INSERT OR IGNORE INTO",
        "notifications",
        &["kind", "payload_json", "dedup_key", "provider_id"],
        chunk.len(),
        Some("RETURNING id, dedup_key"),
    );
    let params = build_notification_chunk_params(kind, chunk)?;
    let mut stmt = conn.prepare(&sql).map_err(crate::error::map_db_error)?;
    let returned_rows = stmt
        .query(rusqlite::params_from_iter(params))
        .map_err(crate::error::map_db_error)?;

    collect_inserted_ids(returned_rows)
}

fn collect_missing_dedup_keys<'a>(
    chunk: &'a [(serde_json::Value, Option<String>, Option<String>)],
    inserted_ids_by_dedup: &std::collections::HashMap<String, i64>,
) -> Vec<&'a str> {
    chunk
        .iter()
        .filter_map(|row| row.1.as_deref())
        .filter(|dk| !inserted_ids_by_dedup.contains_key(*dk))
        .collect()
}

fn fetch_existing_dedup_ids(
    conn: &Connection,
    kind: &str,
    dedup_keys: &[&str],
) -> Result<std::collections::HashMap<String, i64>> {
    let missing_rows: Vec<(i64, String)> = crate::batch::query_in_chunks_with_params(
        conn,
        notification_dedup_select!(
            "WHERE kind = ? AND dedup_key IN ({}) AND date(created_at) = date('now')"
        ),
        &[&kind as &dyn rusqlite::ToSql],
        dedup_keys,
        crate::batch::DEFAULT_CHUNK_SIZE,
        |r| crate::map_row_tuple!(r => (0, 1)),
    )
    .map_err(crate::error::map_db_error)?;

    Ok(missing_rows.into_iter().map(|(id, dk)| (dk, id)).collect())
}

fn resolve_notification_row_id(
    row: &(serde_json::Value, Option<String>, Option<String>),
    inserted_ids_by_dedup: &std::collections::HashMap<String, i64>,
    existing_ids: &std::collections::HashMap<String, i64>,
    inserted_ids_no_dedup: &[i64],
    no_dedup_idx: &mut usize,
) -> Option<i64> {
    if let Some(dk) = &row.1 {
        inserted_ids_by_dedup
            .get(dk)
            .copied()
            .or_else(|| existing_ids.get(dk).copied())
    } else {
        let id = inserted_ids_no_dedup.get(*no_dedup_idx).copied();
        *no_dedup_idx += 1;
        id
    }
}

fn process_notification_chunk(
    conn: &Connection,
    kind: &str,
    chunk: &[(serde_json::Value, Option<String>, Option<String>)],
    all_results: &mut Vec<(i64, serde_json::Value)>,
) -> Result<()> {
    let (inserted_by_dedup, inserted_no_dedup) = execute_insert_chunk(conn, kind, chunk)?;
    let missing_keys = collect_missing_dedup_keys(chunk, &inserted_by_dedup);
    let existing_ids = if missing_keys.is_empty() {
        std::collections::HashMap::new()
    } else {
        fetch_existing_dedup_ids(conn, kind, &missing_keys)?
    };

    let mut no_dedup_idx = 0;
    for row in chunk {
        if let Some(id) = resolve_notification_row_id(
            row,
            &inserted_by_dedup,
            &existing_ids,
            &inserted_no_dedup,
            &mut no_dedup_idx,
        ) {
            all_results.push((id, row.0.clone()));
        }
    }
    Ok(())
}

/// Insert multiple notification rows. Uses `INSERT OR IGNORE` and batching.
/// Returns a Vec of `(id, payload)` matching the inserted/deduped rows.
pub fn insert_many(
    conn: &Connection,
    kind: &str,
    rows: &[(serde_json::Value, Option<String>, Option<String>)], // (payload, dedup_key, provider_id)
) -> Result<Vec<(i64, serde_json::Value)>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let mut all_results = Vec::with_capacity(rows.len());

    let chunk_size =
        (crate::batch::SQLITE_MAX_VARIABLE_NUMBER / 4).clamp(1, crate::batch::DEFAULT_CHUNK_SIZE);
    for chunk in rows.chunks(chunk_size) {
        process_notification_chunk(conn, kind, chunk, &mut all_results)?;
    }

    Ok(all_results)
}

/// Get created_at timestamp for a notification ID.
pub fn get_created_at(conn: &Connection, id: i64) -> Result<Option<String>> {
    conn.query_row(
        notification_created_at_select!("WHERE id = ?1"),
        params![id],
        |row| row.get(0),
    )
    .optional()
    .map_err(crate::error::map_db_error_ctx(
        "get notification created_at",
    ))
}

/// List notifications, most recent first (by descending id).
///
/// - `unread_only`: if `true`, filter to `read_at IS NULL`.
/// - `limit`: max rows to return, clamped to `[1, 200]`.
/// - `before_id`: for cursor pagination — only return rows with `id < before_id`.
///
/// Archived rows (`archived_at IS NOT NULL`) are always excluded.
pub fn list(
    conn: &Connection,
    unread_only: bool,
    limit: i64,
    before_id: Option<i64>,
) -> Result<Vec<NotificationRow>> {
    let limit = limit.clamp(1, 200);
    let sql = if unread_only {
        notification_select!(
            "WHERE archived_at IS NULL AND read_at IS NULL \
             AND id < COALESCE(?1, 9223372036854775807) \
             ORDER BY id DESC LIMIT ?2"
        )
    } else {
        notification_select!(
            "WHERE archived_at IS NULL \
             AND id < COALESCE(?1, 9223372036854775807) \
             ORDER BY id DESC LIMIT ?2"
        )
    };

    crate::db_query_all!(conn, sql, params![before_id, limit], "list notifications")
}

/// Count unread, non-archived notifications.
pub fn unread_count(conn: &Connection) -> Result<i64> {
    let count: Option<i64> = crate::db_query_one!(
        conn,
        "SELECT COUNT(*) FROM notifications \
         WHERE read_at IS NULL AND archived_at IS NULL",
        [],
        |row| row.get(0),
        "unread_count"
    )?;
    Ok(count.unwrap_or(0))
}

/// Mark a single notification as read (sets `read_at` to now). Idempotent.
pub fn mark_read(conn: &Connection, id: i64) -> Result<()> {
    crate::db_execute!(
        conn,
        "UPDATE notifications SET read_at = datetime('now') WHERE id = ?1 AND read_at IS NULL",
        params![id],
        "mark_read"
    )?;
    Ok(())
}

/// Mark all unread, non-archived notifications as read. Returns the number of rows updated.
pub fn mark_all_read(conn: &Connection) -> Result<usize> {
    crate::db_execute!(
        conn,
        "UPDATE notifications SET read_at = datetime('now') \
         WHERE read_at IS NULL AND archived_at IS NULL",
        [],
        "mark_all_read"
    )
}

/// Archive all non-archived notifications (sets `archived_at` to now).
/// Returns the number of rows updated.
pub fn archive_all(conn: &Connection) -> Result<usize> {
    crate::db_execute!(
        conn,
        "UPDATE notifications SET archived_at = datetime('now') \
         WHERE archived_at IS NULL",
        [],
        "archive_all"
    )
}

/// Archive a single notification (sets `archived_at` to now).
pub fn archive(conn: &Connection, id: i64) -> Result<()> {
    crate::db_execute!(
        conn,
        "UPDATE notifications SET archived_at = datetime('now') \
         WHERE id = ?1 AND archived_at IS NULL",
        params![id],
        "archive"
    )?;
    Ok(())
}

/// Permanently delete a notification.
pub fn delete(conn: &Connection, id: i64) -> Result<bool> {
    let changed = crate::db_execute!(
        conn,
        "DELETE FROM notifications \
         WHERE id = ?1 AND ( \
             kind = 'system' \
             OR created_at < datetime('now', '-30 days') \
         )",
        params![id],
        "delete notification"
    )?;
    Ok(changed > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::migrations::run(&mut conn).unwrap();
        conn
    }

    #[test]
    fn insert_and_dedup() {
        let conn = fresh_db();
        let payload = serde_json::json!({"provider_id":"p1","model_id":"m1"});
        let id1 = insert(&conn, "model_new", &payload, Some("p1:m1"), Some("p1")).unwrap();
        let id2 = insert(&conn, "model_new", &payload, Some("p1:m1"), Some("p1")).unwrap();
        assert!(id1.is_some());
        assert_eq!(id1, id2);
    }

    #[test]
    fn unread_count_and_read() {
        let conn = fresh_db();
        assert_eq!(unread_count(&conn).unwrap(), 0);
        insert(
            &conn,
            "model_new",
            &serde_json::json!({}),
            Some("p1:m1"),
            Some("p1"),
        )
        .unwrap();
        insert(
            &conn,
            "model_new",
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
    fn mark_all_read_skips_archived() {
        let conn = fresh_db();
        let id_active = insert(
            &conn,
            "model_new",
            &serde_json::json!({}),
            Some("p1:active"),
            Some("p1"),
        )
        .unwrap()
        .unwrap();
        let id_archived = insert(
            &conn,
            "model_new",
            &serde_json::json!({}),
            Some("p1:archived"),
            Some("p1"),
        )
        .unwrap()
        .unwrap();
        archive(&conn, id_archived).unwrap();
        let changed = mark_all_read(&conn).unwrap();
        assert_eq!(changed, 1);
        assert_eq!(unread_count(&conn).unwrap(), 0);

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
        assert!(active_read_at.is_some());
        assert!(archived_read_at.is_none());
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

        let inserted = insert_many(&conn, "model_new", &rows).unwrap();
        assert_eq!(inserted.len(), count);

        let reinserted = insert_many(&conn, "model_new", &rows).unwrap();
        assert_eq!(reinserted.len(), count);
        assert_eq!(inserted, reinserted);
    }
}
