//! Unary usage persistence, split out of the admission policy.
//!
//! `admit_job` + `replay_pending` is `process_job` split in two on purpose: the
//! policy has to know whether a durable copy of the charge exists before
//! choosing between a retry marker and trusting the journal.

use super::db_port::UnaryPersistError;
use openproxy_types::UsageInput;
use openproxy_types::error::CoreError;
use rusqlite::Connection;
use std::sync::Arc;

/// Append a unary usage job to the journal and apply it, telling the two
/// failure modes apart.
///
/// `admit_job` + `replay_pending` is `process_job` split in two on purpose: the
/// policy has to know whether a durable copy exists before deciding between a
/// retry marker and trusting the journal.
pub(super) fn persist_unary_usage(
    conn: &Arc<parking_lot::Mutex<Connection>>,
    usage: &UsageInput,
) -> std::result::Result<(), UnaryPersistError> {
    // An earlier attempt may have inserted the row and failed afterwards (for
    // example a panic between append and apply). Inserting the same row twice
    // would debit twice, since nothing in the schema dedupes request ids.
    if unary_usage_row_exists(conn, usage)? {
        return Ok(());
    }
    let job = crate::worker::BackgroundJob::RecordUsage(Box::new(usage.clone()));
    if let Err(error) = crate::worker::admit_job(conn, &job) {
        return Err(UnaryPersistError::Lost(error));
    }
    crate::worker::replay_pending(conn).map_err(UnaryPersistError::Journaled)
}

fn unary_usage_row_exists(
    conn: &Arc<parking_lot::Mutex<Connection>>,
    usage: &UsageInput,
) -> std::result::Result<bool, UnaryPersistError> {
    let found: i64 = conn
        .lock()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM usage WHERE request_id = ?1 AND attempt = ?2)",
            rusqlite::params![usage.request_id.to_string(), i64::from(usage.attempt)],
            |row| row.get(0),
        )
        .map_err(|error| {
            UnaryPersistError::Lost(CoreError::Internal(format!(
                "unary usage lookup failed: {error}"
            )))
        })?;
    Ok(found != 0)
}
