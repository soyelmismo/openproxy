use super::*;
use axum::{
    Json,
    extract::{Query, State},
};

pub async fn debug_logs(
    State(_s): State<AppState>,
    Query(q): Query<DebugLogsQuery>,
) -> ApiResult<Json<DebugLogsResponse>> {
    let since = q.since.unwrap_or(0);
    let limit = q.limit.unwrap_or(100).min(1000) as usize;

    // Snapshot from the ring buffer.
    let mut entries = if since > 0 {
        crate::debug_log::snapshot_since(since)
    } else {
        crate::debug_log::snapshot()
    };

    // Apply filters.
    if let Some(rid) = &q.request_id {
        entries.retain(|e| e.request_id.as_deref() == Some(rid.as_str()));
    }
    if let Some(tid) = &q.trace_id {
        entries.retain(|e| e.trace_id.as_deref() == Some(tid.as_str()));
    }
    if let Some(lvl) = &q.level {
        let wanted: std::collections::HashSet<String> = lvl
            .split(',')
            .map(|s| s.trim().to_ascii_uppercase())
            .collect();
        entries.retain(|e| wanted.contains(&e.level.to_ascii_uppercase()));
    }

    let total_in_buffer = entries.len();
    // Truncate to `limit` (keep the most recent — the buffer is
    // oldest-first, so truncate from the front).
    if entries.len() > limit {
        let drop = entries.len() - limit;
        entries.drain(0..drop);
    }

    let latest_seq = entries.last().map(|e| e.seq).unwrap_or(since);

    ApiResult::ok(Json(DebugLogsResponse {
        entries,
        latest_seq,
        total_in_buffer,
    }))
}

pub async fn debug_logs_clear(State(_s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    crate::debug_log::clear();
    ApiResult::ok(Json(serde_json::json!({ "cleared": true })))
}

pub async fn debug_vacuum(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    s.set_vacuum_in_progress(true);
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        // Step 0: Reopen both connections BEFORE attempting VACUUM.
        // The long-lived writer + reader connections hold stale page
        // caches that reference pages from the pre-repair DB file.
        // After an offline DB repair (sqlite3 .recover), the file on
        // disk is completely different but the in-process connections
        // still see the old file. Reopening gives us fresh connections
        // that see the current state of the DB file.
        tracing::info!("VACUUM step 0: reopening DB connections to clear stale page cache");
        if let Err(e) = s.db_pool().reopen() {
            tracing::warn!(error = %e, "VACUUM step 0: reopen failed (continuing with existing connection)");
        }
        // Drop the old writer guard — reopen() took its own locks
        // internally. Now acquire a fresh writer for the VACUUM.

        let w = s
            .db_pool()
            .try_writer_for(ADMIN_LOCK_TIMEOUT)
            .ok_or_else(|| {
                ApiError(CoreError::ServiceUnavailable(
                    "writer lock busy: cannot VACUUM while another write is in progress".into(),
                ))
            })?;

        // Step 1: Checkpoint the WAL.
        let _ = w.pragma_update(None, "wal_checkpoint", "TRUNCATE");
        tracing::info!("VACUUM step 1: WAL checkpoint done");

        // Step 2: Integrity check.
        let integrity: String = w
            .query_row("PRAGMA integrity_check;", [], |r| r.get::<_, String>(0))
            .unwrap_or_else(|e| format!("integrity_check error: {}", e));
        tracing::info!("VACUUM step 2: integrity_check = {}", integrity);

        if integrity != "ok" {
            let _ = w.pragma_update(None, "auto_vacuum", "INCREMENTAL");
            let inc_result = w.execute_batch("PRAGMA incremental_vacuum(1000);");
            match inc_result {
                Ok(()) => {
                    tracing::info!("VACUUM: incremental_vacuum succeeded despite integrity issues");
                    // Reopen connections so subsequent queries see the
                    // compacted DB.
                    drop(w);
                    let _ = s.db_pool().reopen();
                    s.record_vacuum_result("partial (integrity issues — incremental only)");
                    return Ok(Json(serde_json::json!({
                        "vacuumed": true,
                        "partial": true,
                        "integrity_check": integrity,
                        "message": "Incremental VACUUM completed, but the database has integrity issues. \
                                    For a full repair, stop the server and run: \
                                    sqlite3 data.db '.recover' > recovered.sql && \
                                    mv data.db data.db.bak && \
                                    sqlite3 data.db < recovered.sql"
                    })));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "VACUUM: incremental_vacuum also failed");
                    s.record_vacuum_result(&format!("failed: {}", e));
                    return Err(ApiError(CoreError::Database {
                        message: format!(
                            "VACUUM failed: {}. The database has integrity issues: {}. \
                             To repair: stop the server and run \
                             'sqlite3 data.db \".recover\" > recovered.sql && \
                             mv data.db data.db.bak && \
                             sqlite3 data.db < recovered.sql'",
                            e, integrity
                        ),
                        source: Some(Box::new(e)),
                    }));
                }
            }
        }

        // Step 3: DB is healthy — run full VACUUM.
        // VACUUM creates a full copy of the DB. We temporarily switch temp_store
        // to FILE to prevent memory exhaustion, as our global temp_store=MEMORY
        // would force the entire VACUUM operation into RAM.
        let _ = w.pragma_update(None, "temp_store", "FILE");
        let vacuum_res = w.execute_batch("VACUUM;");
        let _ = w.pragma_update(None, "temp_store", "MEMORY");

        match vacuum_res {
            Ok(()) => {
                tracing::info!("VACUUM step 3: full VACUUM completed");
                // Reopen connections so subsequent queries see the
                // compacted DB (VACUUM rebuilds the file; the old
                // connection's page cache is stale).
                drop(w);
                let _ = s.db_pool().reopen();
                s.record_vacuum_result("ok");
                Ok(Json(serde_json::json!({
                    "vacuumed": true,
                    "integrity_check": "ok",
                    "message": "VACUUM completed. Free pages have been reclaimed. \
                                DB connections reopened to refresh page cache."
                })))
            }
            Err(e) => {
                tracing::warn!(error = %e, "VACUUM step 3: full VACUUM failed, trying incremental");
                let _ = w.pragma_update(None, "auto_vacuum", "INCREMENTAL");
                match w.execute_batch("PRAGMA incremental_vacuum(1000);") {
                    Ok(()) => {
                        tracing::info!("VACUUM: incremental fallback succeeded");
                        drop(w);
                        let _ = s.db_pool().reopen();
                        s.record_vacuum_result("partial (full VACUUM failed, incremental fallback)");
                        Ok(Json(serde_json::json!({
                            "vacuumed": true,
                            "partial": true,
                            "message": "Full VACUUM failed but incremental reclaim succeeded. \
                                        DB connections have been reopened. \
                                        The database is usable — try a full VACUUM again later \
                                        or restart the server for a clean state."
                        })))
                    }
                    Err(e2) => {
                        tracing::warn!(error = %e2, "VACUUM: both full and incremental failed");
                        s.record_vacuum_result(&format!("failed: {}", e2));
                        Err(ApiError(CoreError::Database {
                            message: format!(
                                "VACUUM failed: {}. The disk may be full or the DB file \
                                 may be locked by another process. Free disk space and retry, \
                                 or restart the server.",
                                e2
                            ),
                            source: Some(Box::new(e2)),
                        }))
                    }
                }
            }
        }
    }
    .await;
    s.set_vacuum_in_progress(false);
    match body {
        Ok(v) => ApiResult::ok(v),
        Err(e) => ApiResult::err(e),
    }
}

pub async fn debug_recover(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    s.set_vacuum_in_progress(true);
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        // We need exclusive access to the DB for the entire repair.
        // Take the writer lock and hold it.
        let w = s
            .db_pool()
            .try_writer_for(std::time::Duration::from_secs(60))
            .ok_or_else(|| {
                ApiError(CoreError::ServiceUnavailable(
                    "writer lock busy: cannot repair while requests are in flight".into(),
                ))
            })?;

        // Step 1: Get the DB path so we can work with the file directly.
        let db_path = s.db_pool().path().to_path_buf();

        // Step 2: Use SQLite's built-in recovery via `.dump` SQL.
        // We can't run `.recover` (it's a sqlite3 CLI command, not SQL),
        // but we can achieve the same effect by:
        //   a) Dumping all tables to a SQL script in memory
        //   b) Closing the current connection
        //   c) Renaming the old DB
        //   d) Creating a fresh DB and replaying the script
        //
        // However, we can't close the connection while holding the
        // MutexGuard. Instead, we'll use a different approach:
        // run `PRAGMA integrity_check` to see what's wrong, then
        // attempt to rebuild each table individually.

        let integrity: String = w
            .query_row("PRAGMA integrity_check;", [], |r| r.get::<_, String>(0))
            .unwrap_or_else(|e| format!("error: {}", e));

        tracing::info!(
            integrity = %integrity,
            db_path = %db_path.display(),
            "DB repair: starting recovery"
        );

        // List all tables so we can rebuild them.
        let mut stmt = w
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .map_err(|e| ApiError(CoreError::Database {
                message: format!("repair: list tables: {}", e),
                source: Some(Box::new(e)),
            }))?;
        let table_names: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| ApiError(CoreError::Database {
                message: format!("repair: query tables: {}", e),
                source: Some(Box::new(e)),
            }))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);

        tracing::info!(
            tables = ?table_names,
            "DB repair: found {} tables to rebuild",
            table_names.len()
        );

        // For each table, try to read all rows and count them.
        // This tells us which tables are readable (not corrupt).
        let mut table_stats: Vec<serde_json::Value> = Vec::new();
        let mut total_rows_recovered: u64 = 0;
        for table in &table_names {
            let count_result: rusqlite::Result<i64> = w.query_row(
                &format!("SELECT COUNT(*) FROM \"{}\"", table),
                [],
                |r| r.get(0),
            );
            match count_result {
                Ok(count) => {
                    total_rows_recovered += count as u64;
                    table_stats.push(serde_json::json!({
                        "table": table,
                        "rows": count,
                        "status": "ok"
                    }));
                }
                Err(e) => {
                    tracing::warn!(
                        table = %table,
                        error = %e,
                        "DB repair: table is unreadable"
                    );
                    table_stats.push(serde_json::json!({
                        "table": table,
                        "rows": 0,
                        "status": "corrupt",
                        "error": e.to_string()
                    }));
                }
            }
        }

        // The actual repair (rebuild the DB file) can't be done
        // from within the process — we'd need to close all
        // connections, rename the file, and create a new one.
        // That requires a server restart. So we return the
        // diagnostic info + instructions.
        s.record_vacuum_result(&format!("recovery diagnostic ({} rows readable)", total_rows_recovered));

        if integrity == "ok" {
            return Ok(Json(serde_json::json!({
                "recovered": false,
                "integrity_check": "ok",
                "message": "Database integrity is OK — no repair needed. \
                            If you're seeing disk I/O errors, the issue may be \
                            disk space or file permissions, not DB corruption."
            })));
        }

        // DB is corrupt. We can't auto-repair from within the process,
        // but we CAN give the operator the exact commands to run.
        Ok(Json(serde_json::json!({
            "recovered": false,
            "needs_manual_repair": true,
            "integrity_check": integrity,
            "tables": table_stats,
            "total_rows_recovered": total_rows_recovered,
            "db_path": db_path.display().to_string(),
            "instructions": format!(
                "The database at {} has corruption. To repair:\n\
                 1. Stop the openproxy server\n\
                 2. Run: sqlite3 {} '.recover' > /tmp/recovered.sql\n\
                 3. Run: mv {} {}.bak\n\
                 4. Run: sqlite3 {} < /tmp/recovered.sql\n\
                 5. Restart the server\n\
                 This will recover all readable rows into a fresh, unfragmented DB.",
                db_path.display(),
                db_path.display(),
                db_path.display(),
                db_path.display(),
                db_path.display()
            )
        })))
    }
    .await;
    s.set_vacuum_in_progress(false);
    match body {
        Ok(v) => ApiResult::ok(v),
        Err(e) => ApiResult::err(e),
    }
}

pub async fn get_recording(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    if let Err(e) = authenticate_admin_ws(&s, &headers, None) {
        return e.into();
    }
    let body: Result<Json<serde_json::Value>, ApiError> =
        async { Ok(Json(serde_json::json!({ "recording": s.is_recording() }))) }.await;
    body.into()
}

pub async fn set_recording(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    if let Err(e) = authenticate_admin_ws(&s, &headers, None) {
        return e.into();
    }
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let enabled = body
            .get("enabled")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| CoreError::Validation("missing 'enabled' bool".into()))?;
        s.set_recording(enabled);
        Ok(Json(serde_json::json!({ "recording": enabled })))
    }
    .await;
    body.into()
}

pub(crate) fn json_text(value: serde_json::Value) -> Result<String, ApiError> {
    serde_json::to_string(&value).map_err(|e| {
        ApiError(CoreError::Internal(format!(
            "serialize websocket message: {e}"
        )))
    })
}
