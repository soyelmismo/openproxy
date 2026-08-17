use super::{ADMIN_LOCK_TIMEOUT, ApiError, AppState, CoreError, Deserialize, Serialize};
use axum::{
    Json,
    extract::{Query, State},
};

/// Query parameters for `GET /admin/debug/logs`.
#[derive(Debug, Default, Deserialize)]
pub struct DebugLogsQuery {
    pub since: Option<u64>,
    pub request_id: Option<String>,
    pub trace_id: Option<String>,
    pub level: Option<String>,
    pub limit: Option<u32>,
}

/// Response envelope for `GET /admin/debug/logs`.
#[derive(Debug, Serialize)]
pub struct DebugLogsResponse {
    pub entries: Vec<crate::debug_log::DebugLogEntry>,
    pub latest_seq: u64,
    pub total_in_buffer: usize,
}

pub async fn debug_logs(
    State(_s): State<AppState>,
    Query(q): Query<DebugLogsQuery>,
) -> Result<Json<DebugLogsResponse>, ApiError> {
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

    let latest_seq = entries.last().map_or(since, |e| e.seq);

    Ok(Json(DebugLogsResponse {
        entries,
        latest_seq,
        total_in_buffer,
    }))
}

pub async fn debug_logs_clear(
    State(_s): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::debug_log::clear();
    Ok(Json(serde_json::json!({ "cleared": true })))
}

pub async fn debug_vacuum(State(s): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    s.set_vacuum_in_progress(true);
    let res: Result<Json<serde_json::Value>, ApiError> = async {

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
        let _ = openproxy_db::maintenance::checkpoint_wal(&w);
        tracing::info!("VACUUM step 1: WAL checkpoint done");

        // Step 2: Integrity check.
        let integrity = openproxy_db::maintenance::integrity_check(&w);
        tracing::info!("VACUUM step 2: integrity_check = {}", integrity);

        if integrity != "ok" {
            let inc_result = openproxy_db::maintenance::incremental_vacuum(&w, 1000);
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
                    s.record_vacuum_result(&format!("failed: {e}"));
                    return Err(ApiError(CoreError::Database {
                        message: format!(
                            "VACUUM failed: {e}. The database has integrity issues: {integrity}. \
                             To repair: stop the server and run \
                             'sqlite3 data.db \".recover\" > recovered.sql && \
                             mv data.db data.db.bak && \
                             sqlite3 data.db < recovered.sql'"
                        ),
                        source: Some(Box::new(e)),
                    }));
                }
            }
        }

        // Step 3: DB is healthy — run full VACUUM.
        let vacuum_res = openproxy_db::maintenance::vacuum(&w);

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
                match openproxy_db::maintenance::incremental_vacuum(&w, 1000) {
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
                        s.record_vacuum_result(&format!("failed: {e2}"));
                        Err(ApiError(CoreError::Database {
                            message: format!(
                                "VACUUM failed: {e2}. The disk may be full or the DB file \
                                 may be locked by another process. Free disk space and retry, \
                                 or restart the server."
                            ),
                            source: Some(Box::new(e2)),
                        }))
                    }
                }
            }
        }

    }.await;
    s.set_vacuum_in_progress(false);
    res
}

pub async fn debug_recover(State(s): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    s.set_vacuum_in_progress(true);
    let res: Result<Json<serde_json::Value>, ApiError> = async {
        // We need exclusive access to the DB for the entire repair.
        // Take the writer lock and hold it.
        let w = s
            .db_pool()
            .try_writer_for(std::time::Duration::from_mins(1))
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

        let integrity = openproxy_db::maintenance::integrity_check(&w);

        tracing::info!(
            integrity = %integrity,
            db_path = %db_path.display(),
            "DB repair: starting recovery"
        );

        // List all tables so we can rebuild them.
        let table_names = openproxy_db::maintenance::list_user_tables(&w).map_err(|e| {
            ApiError(CoreError::Database {
                message: format!("repair: list tables: {e}"),
                source: Some(Box::new(e)),
            })
        })?;

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
            let count_result = openproxy_db::maintenance::count_table_rows(&w, table);
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
                        "error": openproxy_core::cost::redact_error_msg(&e.to_string()).0
                    }));
                }
            }
        }

        // The actual repair (rebuild the DB file) can't be done
        // from within the process — we'd need to close all
        // connections, rename the file, and create a new one.
        // That requires a server restart. So we return the
        // diagnostic info + instructions.
        s.record_vacuum_result(&format!(
            "recovery diagnostic ({total_rows_recovered} rows readable)"
        ));

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
    res
}

pub async fn get_recording(
    _auth: crate::extractors::AdminAuth,
    State(s): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(serde_json::json!({ "recording": s.is_recording() })))
}

pub async fn set_recording(
    _auth: crate::extractors::AdminAuth,
    State(s): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let enabled = body
        .get("enabled")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| CoreError::Validation("missing 'enabled' bool".into()))?;
    s.set_recording(enabled);
    Ok(Json(serde_json::json!({ "recording": enabled })))
}

pub(crate) fn json_text(value: &serde_json::Value) -> Result<String, ApiError> {
    serde_json::to_string(value).map_err(|e| {
        ApiError(CoreError::Internal(format!(
            "serialize websocket message: {e}"
        )))
    })
}
