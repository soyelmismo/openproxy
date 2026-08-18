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
            .optional()
            .map_err(crate::error::map_db_error_ctx(
                "query dedup notification id",
            ))?
        } else {
            None
        };
        Ok(existing)
    } else {
        Ok(Some(conn.last_insert_rowid()))
    }
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
        let sql = crate::batch::build_insert_sql(
            "INSERT OR IGNORE INTO",
            "notifications",
            &["kind", "payload_json", "dedup_key", "provider_id"],
            chunk.len(),
            Some("RETURNING id, dedup_key"),
        );
        let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len() * 4);

        for row in chunk {
            params.push(kind.to_owned().into());
            let payload_str = serde_json::to_string(&row.0).map_err(|e| {
                openproxy_types::error::CoreError::Validation(format!(
                    "serialize notification payload: {e}"
                ))
            })?;
            params.push(payload_str.into());
            match &row.1 {
                Some(k) => params.push(k.to_owned().into()),
                None => params.push(rusqlite::types::Value::Null),
            }
            match &row.2 {
                Some(p) => params.push(p.to_owned().into()),
                None => params.push(rusqlite::types::Value::Null),
            }
        }

        let mut stmt = conn.prepare(&sql).map_err(crate::error::map_db_error)?;
        let mut returned_rows = stmt
            .query(rusqlite::params_from_iter(params))
            .map_err(crate::error::map_db_error)?;

        let mut inserted_ids_by_dedup = std::collections::HashMap::new();
        let mut inserted_ids_no_dedup = Vec::new();

        while let Some(r) = returned_rows.next().map_err(crate::error::map_db_error)? {
            let id: i64 = r.get(0).map_err(crate::error::map_db_error)?;
            let dedup_key: Option<String> = r.get(1).map_err(crate::error::map_db_error)?;
            if let Some(dk) = dedup_key {
                inserted_ids_by_dedup.insert(dk, id);
            } else {
                inserted_ids_no_dedup.push(id);
            }
        }

        let mut no_dedup_idx = 0;
        let mut missing_dedup_keys = Vec::new();

        // Pass 1: map inserted rows and collect missing dedups
        for (i, row) in chunk.iter().enumerate() {
            if let Some(dk) = &row.1
                && !inserted_ids_by_dedup.contains_key(dk)
            {
                missing_dedup_keys.push((i, dk.to_owned()));
            }
        }

        let mut existing_ids = std::collections::HashMap::new();
        if !missing_dedup_keys.is_empty() {
            let dedup_keys: Vec<&str> = missing_dedup_keys
                .iter()
                .map(|(_, dk)| dk.as_str())
                .collect();
            let missing_rows: Vec<(i64, String)> =
                crate::batch::query_in_chunks_with_params(
                    conn,
                    "SELECT id, dedup_key FROM notifications WHERE kind = ? AND dedup_key IN ({}) AND date(created_at) = date('now')",
                    &[&kind as &dyn rusqlite::ToSql],
                    &dedup_keys,
                    crate::batch::DEFAULT_CHUNK_SIZE,
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(crate::error::map_db_error)?;
            for (id, dk) in missing_rows {
                existing_ids.insert(dk, id);
            }
        }

        // Pass 2: resolve all IDs
        for row in chunk {
            let id = if let Some(dk) = &row.1 {
                if let Some(&inserted_id) = inserted_ids_by_dedup.get(dk) {
                    Some(inserted_id)
                } else if let Some(&existing_id) = existing_ids.get(dk) {
                    Some(existing_id)
                } else {
                    None
                }
            } else {
                let id = inserted_ids_no_dedup.get(no_dedup_idx).copied();
                no_dedup_idx += 1;
                id
            };

            if let Some(id) = id {
                all_results.push((id, row.0.clone()));
            }
        }
    }

    Ok(all_results)
}

/// Get created_at timestamp for a notification ID.
pub fn get_created_at(conn: &Connection, id: i64) -> Result<Option<String>> {
    conn.query_row(
        "SELECT created_at FROM notifications WHERE id = ?1",
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
    let sql =
        format!(
        "SELECT id, kind, payload_json, read_at, archived_at, created_at, dedup_key, provider_id
         FROM notifications
         WHERE archived_at IS NULL{unread}
           AND id < COALESCE(:before, 9223372036854775807)
         ORDER BY id DESC LIMIT :limit",
        unread = if unread_only { " AND read_at IS NULL" } else { "" }
    );

    let mut stmt = conn.prepare(&sql).map_err(crate::error::map_db_error)?;
    let rows = stmt
        .query_map(
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
        )
        .map_err(crate::error::map_db_error)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(crate::error::map_db_error)?);
    }
    Ok(out)
}

/// Count unread, non-archived notifications.
pub fn unread_count(conn: &Connection) -> Result<i64> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM notifications
             WHERE read_at IS NULL AND archived_at IS NULL",
            [],
            |row| row.get(0),
        )
        .map_err(crate::error::map_db_error_ctx("unread_count"))?;
    Ok(count)
}

/// Mark a single notification as read (sets `read_at` to now). Idempotent.
pub fn mark_read(conn: &Connection, id: i64) -> Result<()> {
    conn.execute(
        "UPDATE notifications SET read_at = datetime('now') WHERE id = ?1 AND read_at IS NULL",
        params![id],
    )
    .map_err(crate::error::map_db_error_ctx("mark_read"))?;
    Ok(())
}

/// Mark all unread, non-archived notifications as read. Returns the number of rows updated.
pub fn mark_all_read(conn: &Connection) -> Result<usize> {
    let changed = conn
        .execute(
            "UPDATE notifications SET read_at = datetime('now')
             WHERE read_at IS NULL AND archived_at IS NULL",
            [],
        )
        .map_err(crate::error::map_db_error_ctx("mark_all_read"))?;
    Ok(changed)
}

/// Archive all non-archived notifications (sets `archived_at` to now).
/// Returns the number of rows updated.
pub fn archive_all(conn: &Connection) -> Result<usize> {
    let changed = conn
        .execute(
            "UPDATE notifications SET archived_at = datetime('now')
             WHERE archived_at IS NULL",
            [],
        )
        .map_err(crate::error::map_db_error_ctx("archive_all"))?;
    Ok(changed)
}

/// Archive a single notification (sets `archived_at` to now).
pub fn archive(conn: &Connection, id: i64) -> Result<()> {
    conn.execute(
        "UPDATE notifications SET archived_at = datetime('now')
         WHERE id = ?1 AND archived_at IS NULL",
        params![id],
    )
    .map_err(crate::error::map_db_error_ctx("archive"))?;
    Ok(())
}

/// Permanently delete a notification.
pub fn delete(conn: &Connection, id: i64) -> Result<bool> {
    let changed = conn
        .execute(
            "DELETE FROM notifications
             WHERE id = ?1 AND (
                 kind = 'system'
                 OR created_at < datetime('now', '-30 days')
             )",
            params![id],
        )
        .map_err(crate::error::map_db_error_ctx("delete notification"))?;
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
