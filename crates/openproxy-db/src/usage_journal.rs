//! Persistent FIFO for usage jobs. Payload contracts belong to the producer.
use openproxy_types::Result;
use rusqlite::{Connection, params};

pub const MAX_PENDING_JOBS: u64 = 100_000;

/// Appends a new payload to the usage journal synchronously.
/// Returns `Err(CoreError::JournalCapacityExhausted)` if `MAX_PENDING_JOBS` is reached.
pub fn append(conn: &mut Connection, payload: &str) -> Result<i64> {
    crate::with_busy_retry("usage_journal::append", || {
        let transaction = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(crate::error::map_db_error)?;
        if depth(&transaction)? >= MAX_PENDING_JOBS {
            return Err(openproxy_types::CoreError::JournalCapacityExhausted);
        }
        transaction
            .execute("INSERT INTO usage_journal(payload) VALUES (?1)", [payload])
            .map_err(crate::error::map_db_error)?;
        let id = transaction.last_insert_rowid();
        transaction.commit().map_err(crate::error::map_db_error)?;
        Ok(id)
    })
}

/// Helper predicate to check whether an error indicates that the usage journal
/// has reached maximum capacity and cannot admit more entries until drained.
#[inline]
#[must_use]
pub fn is_capacity_exhausted(err: &openproxy_types::CoreError) -> bool {
    err.is_journal_capacity_exhausted()
}

pub fn pending(conn: &Connection, limit: usize) -> Result<Vec<(i64, String)>> {
    let mut statement = conn
        .prepare("SELECT id, payload FROM usage_journal ORDER BY id LIMIT ?1")
        .map_err(crate::error::map_db_error)?;
    statement
        .query_map(params![limit as i64], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(crate::error::map_db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(crate::error::map_db_error)
}

/// Must share the transaction that applies the job, never acknowledge first.
pub fn acknowledge(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM usage_journal WHERE id = ?1", [id])
        .map_err(crate::error::map_db_error)?;
    Ok(())
}

pub fn contains(conn: &Connection, id: i64) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM usage_journal WHERE id = ?1)",
        [id],
        |row| row.get(0),
    )
    .map_err(crate::error::map_db_error)
}

pub fn depth(conn: &Connection) -> Result<u64> {
    conn.query_row("SELECT count(*) FROM usage_journal", [], |row| {
        row.get::<_, i64>(0).map(|n| n as u64)
    })
    .map_err(crate::error::map_db_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_jobs_survive_reopening_and_keep_fifo_order() {
        let (pool, path) = crate::testing::fresh_pool();
        let first = append(&mut pool.writer(), "first").unwrap();
        let second = append(&mut pool.writer(), "second").unwrap();
        let reopened = Connection::open(path).unwrap();
        assert_eq!(
            pending(&reopened, 32).unwrap(),
            vec![(first, "first".into()), (second, "second".into())]
        );
    }

    #[test]
    fn journal_is_bounded_without_overwriting_entries() {
        let mut conn = crate::testing::open_in_memory();
        conn.execute_batch(
            "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<100000)
            INSERT INTO usage_journal(payload) SELECT 'pending' FROM n;",
        )
        .unwrap();
        let err = append(&mut conn, "overflow").unwrap_err();
        assert!(is_capacity_exhausted(&err));
        assert!(err.is_journal_capacity_exhausted());
        assert_eq!(
            err.to_string(),
            "usage journal capacity exhausted; admission rejected"
        );
        assert_eq!(depth(&conn).unwrap(), MAX_PENDING_JOBS);
    }
}
