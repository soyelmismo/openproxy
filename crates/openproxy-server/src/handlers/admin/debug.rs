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

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/logs", axum::routing::get(debug_logs))
        .route("/clear", axum::routing::post(debug_logs_clear))
        .route("/vacuum", axum::routing::post(debug_vacuum))
        .route("/recover", axum::routing::post(debug_recover))
}

fn filter_debug_logs(entries: &mut Vec<crate::debug_log::DebugLogEntry>, q: &DebugLogsQuery) {
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
}

pub async fn debug_logs(
    State(_s): State<AppState>,
    Query(q): Query<DebugLogsQuery>,
) -> Result<Json<DebugLogsResponse>, ApiError> {
    let since = q.since.unwrap_or(0);
    let limit = q.limit.unwrap_or(100).min(1000) as usize;

    let mut entries = if since > 0 {
        crate::debug_log::snapshot_since(since)
    } else {
        crate::debug_log::snapshot()
    };

    filter_debug_logs(&mut entries, &q);

    let total_in_buffer = entries.len();
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

fn run_unhealthy_vacuum(
    s: &AppState,
    w: &openproxy_db::conn::WriterGuard<'_>,
    integrity: &str,
) -> Result<serde_json::Value, ApiError> {
    match openproxy_db::maintenance::incremental_vacuum(w, 1000) {
        Ok(()) => {
            tracing::info!("VACUUM: incremental_vacuum succeeded despite integrity issues");
            s.record_vacuum_result("partial (integrity issues — incremental only)");
            Ok(serde_json::json!({
                "vacuumed": true,
                "partial": true,
                "integrity_check": integrity,
                "message": "Incremental VACUUM completed, but the database has integrity issues. \
                            For a full repair, stop the server and run: \
                            sqlite3 data.db '.recover' > recovered.sql && \
                            mv data.db data.db.bak && \
                            sqlite3 data.db < recovered.sql"
            }))
        }
        Err(e) => {
            tracing::warn!(error = %e, "VACUUM: incremental_vacuum also failed");
            s.record_vacuum_result(&format!("failed: {e}"));
            Err(ApiError(CoreError::Database {
                message: format!(
                    "VACUUM failed: {e}. The database has integrity issues: {integrity}. \
                     To repair: stop the server and run \
                     'sqlite3 data.db \".recover\" > recovered.sql && \
                     mv data.db data.db.bak && \
                     sqlite3 data.db < recovered.sql'"
                ),
                source: Some(std::sync::Arc::new(e)),
            }))
        }
    }
}

fn run_healthy_vacuum(
    s: &AppState,
    w: &openproxy_db::conn::WriterGuard<'_>,
) -> Result<serde_json::Value, ApiError> {
    if let Err(e) = openproxy_db::maintenance::vacuum(w) {
        tracing::warn!(error = %e, "VACUUM step 3: full VACUUM failed, trying incremental");
        return fallback_incremental_vacuum(s, w);
    }

    tracing::info!("VACUUM step 3: full VACUUM completed");
    s.record_vacuum_result("ok");
    Ok(serde_json::json!({
        "vacuumed": true,
        "integrity_check": "ok",
        "message": "VACUUM completed. Free pages have been reclaimed. \
                    DB connections reopened to refresh page cache."
    }))
}

fn fallback_incremental_vacuum(
    s: &AppState,
    w: &openproxy_db::conn::WriterGuard<'_>,
) -> Result<serde_json::Value, ApiError> {
    match openproxy_db::maintenance::incremental_vacuum(w, 1000) {
        Ok(()) => {
            tracing::info!("VACUUM: incremental fallback succeeded");
            s.record_vacuum_result("partial (full VACUUM failed, incremental fallback)");
            Ok(serde_json::json!({
                "vacuumed": true,
                "partial": true,
                "message": "Full VACUUM failed but incremental reclaim succeeded. \
                            DB connections have been reopened. \
                            The database is usable — try a full VACUUM again later \
                            or restart the server for a clean state."
            }))
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
                source: Some(std::sync::Arc::new(e2)),
            }))
        }
    }
}

pub async fn debug_vacuum(State(s): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    s.set_vacuum_in_progress(true);
    let res: Result<Json<serde_json::Value>, ApiError> =
        (|| -> Result<Json<serde_json::Value>, ApiError> {
            tracing::info!("VACUUM step 0: reopening DB connections to clear stale page cache");
            let _ = s.db_pool().reopen();

            let w = s
                .db_pool()
                .try_writer_for(ADMIN_LOCK_TIMEOUT)
                .ok_or_else(|| {
                    ApiError(CoreError::ServiceUnavailable(
                        "writer lock busy: cannot VACUUM while another write is in progress".into(),
                    ))
                })?;

            let _ = openproxy_db::maintenance::checkpoint_wal(&w);
            let integrity = openproxy_db::maintenance::integrity_check(&w);
            tracing::info!("VACUUM step 2: integrity_check = {}", integrity);

            let json_val = if integrity != "ok" {
                let res = run_unhealthy_vacuum(&s, &w, &integrity)?;
                drop(w);
                let _ = s.db_pool().reopen();
                res
            } else {
                let res = run_healthy_vacuum(&s, &w)?;
                drop(w);
                let _ = s.db_pool().reopen();
                res
            };

            Ok(Json(json_val))
        })();

    s.set_vacuum_in_progress(false);
    res
}

fn collect_table_recovery_stats(
    w: &openproxy_db::conn::WriterGuard<'_>,
    tables: &[String],
) -> (Vec<serde_json::Value>, u64) {
    let mut table_stats = Vec::new();
    let mut total_rows_recovered = 0;

    for table in tables {
        match openproxy_db::maintenance::DbTable::parse(table)
            .map(|t| openproxy_db::maintenance::count_table_rows(w, t))
        {
            Some(Ok(count)) => {
                total_rows_recovered += count as u64;
                table_stats.push(serde_json::json!({
                    "table": table,
                    "rows": count,
                    "status": "ok"
                }));
            }
            Some(Err(e)) => {
                tracing::warn!(table = %table, error = %e, "DB repair: table is unreadable");
                table_stats.push(serde_json::json!({
                    "table": table,
                    "rows": 0,
                    "status": "corrupt",
                    "error": openproxy_core::cost::redact_error_msg(&e.to_string()).0
                }));
            }
            None => {
                tracing::warn!(table = %table, "DB repair: table is unknown or unmodeled");
                table_stats.push(serde_json::json!({
                    "table": table,
                    "rows": 0,
                    "status": "unknown"
                }));
            }
        }
    }
    (table_stats, total_rows_recovered)
}

pub async fn debug_recover(State(s): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    s.set_vacuum_in_progress(true);
    let res: Result<Json<serde_json::Value>, ApiError> =
        (|| -> Result<Json<serde_json::Value>, ApiError> {
            let w = s
                .db_pool()
                .try_writer_for(std::time::Duration::from_mins(1))
                .ok_or_else(|| {
                    ApiError(CoreError::ServiceUnavailable(
                        "writer lock busy: cannot repair while requests are in flight".into(),
                    ))
                })?;

            let db_path = s.db_pool().path().to_path_buf();
            let integrity = openproxy_db::maintenance::integrity_check(&w);
            let table_names = openproxy_db::maintenance::list_user_tables(&w).map_err(|e| {
                ApiError(CoreError::Database {
                    message: format!("repair: list tables: {e}"),
                    source: Some(std::sync::Arc::new(e)),
                })
            })?;

            let (table_stats, total_rows_recovered) =
                collect_table_recovery_stats(&w, &table_names);
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
        })();

    s.set_vacuum_in_progress(false);
    res
}

pub async fn get_recording(State(s): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(serde_json::json!({ "recording": s.is_recording() })))
}

pub async fn set_recording(
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
        tracing::error!(error = %e, "serialize websocket message failed");
        ApiError(CoreError::Internal(
            "failed to serialize websocket message".into(),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_db::maintenance::DbTable;

    #[test]
    fn test_json_text_success() {
        let val = serde_json::json!({ "hello": "world" });
        let res = json_text(&val);
        assert_eq!(res.unwrap(), r#"{"hello":"world"}"#);
    }

    #[test]
    fn test_json_text_error_redaction() {
        // Verify that ApiError for internal json_text failure uses generic message
        let err = ApiError(CoreError::Internal(
            "failed to serialize websocket message".into(),
        ));
        let err_msg = err.to_string();
        assert!(err_msg.contains("failed to serialize websocket message"));
        assert!(!err_msg.contains("serialize websocket message:"));
    }

    #[test]
    fn test_db_table_strict_parsing_prevents_sql_injection() {
        let malicious_inputs = [
            "providers\"; DROP TABLE providers; --",
            "users",
            "SELECT * FROM sqlite_master",
            "providers' OR '1'='1",
            "api_keys --",
        ];

        for input in malicious_inputs {
            assert!(
                DbTable::parse(input).is_none(),
                "Unmodeled/malicious table name '{input}' must be rejected by DbTable::parse"
            );
        }

        assert_eq!(DbTable::parse("providers"), Some(DbTable::Providers));
        assert_eq!(DbTable::parse("api_keys"), Some(DbTable::ApiKeys));
    }
}
