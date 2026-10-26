//! Opt-in process-wide SQL statement counters. Never enabled in normal builds.
//! Counts executed statements, not pool acquisitions or guessed query counts.

use rusqlite::trace::{TraceEvent, TraceEventCodes};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

static ACTIVE: AtomicBool = AtomicBool::new(false);
static COUNTS: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());

pub(crate) fn install(conn: &rusqlite::Connection) {
    conn.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(on_statement));
}

fn on_statement(event: TraceEvent<'_>) {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    if let TraceEvent::Stmt(stmt, _) = event {
        // sqlite3_sql, NOT expanded_sql: never collect bound credentials/payloads.
        let sql = stmt.sql();
        let mut counts = COUNTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A callback may have waited for this lock while stop() disabled
        // collection. Do not mutate a snapshot after its window has closed.
        if !ACTIVE.load(Ordering::Relaxed) {
            return;
        }
        if counts.contains_key(sql.as_ref()) || counts.len() < 512 {
            *counts.entry(sql.into_owned()).or_default() += 1;
        } else {
            *counts.entry("[other statements]".into()).or_default() += 1;
        }
    }
}

#[derive(Serialize)]
pub struct Snapshot {
    pub statements: u64,
    pub by_prepared_sql: BTreeMap<String, u64>,
}

/// Start only after setup/warmup have completed and their usage has drained.
pub fn start() {
    COUNTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    ACTIVE.store(true, Ordering::Relaxed);
}

/// Include the usage-worker drain in this interval. Background SQL is included.
pub fn stop() -> Snapshot {
    ACTIVE.store(false, Ordering::Relaxed);
    let counts = COUNTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    Snapshot {
        statements: counts.values().sum(),
        by_prepared_sql: counts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_execution_on_every_pool_connection_without_expanding_bindings() {
        let pool = crate::DbPool::test_pool().unwrap();
        start();
        pool.writer()
            .query_row("SELECT ?1", ["secret-credential"], |r| {
                r.get::<_, String>(0)
            })
            .unwrap();
        pool.reader()
            .query_row("SELECT 2", [], |r| r.get::<_, i64>(0))
            .unwrap();
        let snapshot = stop();
        assert!(snapshot.statements >= 2);
        assert!(snapshot.by_prepared_sql.contains_key("SELECT ?1"));
        assert!(!format!("{:?}", snapshot.by_prepared_sql).contains("secret-credential"));
        pool.reader()
            .query_row("SELECT 3", [], |r| r.get::<_, i64>(0))
            .unwrap();
        assert_eq!(stop().statements, snapshot.statements);
    }
}
