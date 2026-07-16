//! SQLite connection pool.
//!
//! MVP design: one writer connection guarded by a Mutex (SQLite serializes writes
//! at the file level anyway, and we want strict serialization to keep migration
//! lock semantics simple per spec §9). Readers are cheap clones of an
//! `Arc<Connection>`; rusqlite's `Connection: Send` but not `Sync`, so readers
//! each get their own clone but share the underlying handle state.
//!
//! This avoids adding `r2d2` / `r2d2_sqlite` deps for the MVP. If we ever need
//! concurrent writers, swap the writer field for a real pool.

use openproxy_types::{CoreError, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use std::sync::Arc;

/// Alias for the writer guard returned by [`DbPool::writer`].
pub type WriterGuard<'a> = parking_lot::MutexGuard<'a, Connection>;

/// Alias for the reader guard returned by [`DbPool::reader`].
pub type ReaderGuard<'a> = parking_lot::MutexGuard<'a, Connection>;

/// Connection pool holding one serialized writer and one serialized reader.
/// SQLite file-level locking + rusqlite's lack of `Sync` on `Connection` mean we
/// guard both with a Mutex. A future r2d2-based pool can swap in true reader
/// concurrency without changing the public API beyond return types.
#[derive(Clone)]
pub struct DbPool {
    writer: Arc<Mutex<Connection>>,
    reader: Arc<Mutex<Connection>>,
    /// Path to the SQLite file the pool was opened against. Used by
    /// [`DbPool::open_connection`] to spin up an *additional* owned
    /// handle on the same handle when a caller needs an owned
    /// `Connection` (rusqlite 0.31's `Connection: !Clone`, so the
    /// only way to get a second handle is to open a new one).
    path: Arc<Path>,
}

/// Time budget for the writer lock on hot-path inserts.
///
/// The hot path is `cost::record`: every chat request takes the
/// writer briefly to persist a usage row. If the writer is held by
/// a long-running admin query (e.g. a 30-day usage summary that
/// touches ~10k rows), every concurrent chat request would block
/// until the admin query finishes. With 100ms ceiling the worst
/// case is a lost usage row (logged + returned as `None`), never
/// a hung client request.
pub const HOT_PATH_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

/// Time budget for the writer lock on admin/dashboard queries. Much
/// longer than the hot path because the operator explicitly asked
/// for the result; we'd rather wait a few seconds than 500.
pub const ADMIN_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Reason a `try_lock` returned `None` instead of a guard. Used by
/// the hot path to log + count dropped writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockTimeout {
    Hot,
    Admin,
}

impl std::fmt::Debug for DbPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbPool").finish_non_exhaustive()
    }
}

impl DbPool {
    /// Open or create a SQLite database at `path`, configure pragmas, and return
    /// a ready-to-use pool. The caller is expected to run migrations on the
    /// writer before issuing any queries.
    pub fn open(path: &Path) -> Result<Self> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE;

        let writer = Connection::open_with_flags(path, flags).map_err(
            crate::error::map_db_error_ctx(format!("open {}", path.display())),
        )?;

        // SQLite defaults to creating temporary files in /tmp or /var/tmp which might
        // not be writable in some containers or could fill up quickly, causing
        // "disk I/O error" during VACUUM or large GROUP BY operations.
        if let Some(parent) = path.parent() {
            let p_str = parent.to_string_lossy();
            if !p_str.is_empty() {
                // This PRAGMA is deprecated but still works and sets the global temp dir
                let p_escaped = p_str.replace("'", "''");
                let _ = writer.execute(
                    &format!("PRAGMA temp_store_directory = '{}'", p_escaped),
                    [],
                );
            }
        }

        configure_connection(&writer)?;

        // Reader: open a second handle on the same file. Cloning the writer
        // would also work, but a fresh open is explicit and avoids sharing any
        // per-connection state that might be written during the writer setup.
        let reader = Connection::open_with_flags(path, flags).map_err(
            crate::error::map_db_error_ctx(format!("open reader {}", path.display())),
        )?;

        configure_connection(&reader)?;

        Ok(Self {
            writer: Arc::new(Mutex::new(writer)),
            reader: Arc::new(Mutex::new(reader)),
            path: Arc::from(path),
        })
    }

    /// Acquire the serialized writer. Blocks until the previous writer is released.
    pub fn writer(&self) -> WriterGuard<'_> {
        self.writer.lock()
    }

    /// Try to acquire the writer lock for at most `timeout` (blocking).
    /// Returns `None` if the lock could not be acquired in time — the
    /// caller decides what to do (drop the write, log + retry, 503 the
    /// request, etc.).
    ///
    /// This is the LOW fix for `db_pool` write-lock starvation: a
    /// long-running admin query holding the writer no longer freezes
    /// the hot path indefinitely.
    pub fn try_writer_for(&self, timeout: std::time::Duration) -> Option<WriterGuard<'_>> {
        self.writer.try_lock_for(timeout)
    }

    /// Clone the writer mutex's [`Arc`] handle. Used by long-lived consumers
    /// (e.g. the request [`crate::pipeline::Pipeline`]) that need to lock
    /// the connection repeatedly without going through the borrow checker
    /// each time. The returned `Arc` is `Clone` and can be moved into
    /// spawned tasks; multiple consumers can hold the same handle and each
    /// `lock()` call serializes as before.
    pub fn writer_arc(&self) -> Arc<Mutex<Connection>> {
        self.writer.clone()
    }

    /// Acquire the serialized reader. Blocks until the previous reader is released.
    pub fn reader(&self) -> ReaderGuard<'_> {
        self.reader.lock()
    }

    /// Try to acquire the reader lock for at most `timeout` (blocking).
    /// Returns `None` if the lock could not be acquired in time. Used by
    /// analytics queries so a long-running reader doesn't block the
    /// admin endpoint indefinitely — the caller returns 503 and the
    /// operator can retry.
    pub fn try_reader_for(&self, timeout: std::time::Duration) -> Option<ReaderGuard<'_>> {
        self.reader.try_lock_for(timeout)
    }

    /// Run a closure against the serialized writer connection.
    pub fn with_conn<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Connection) -> R,
    {
        let guard = self.writer.lock();
        f(&guard)
    }

    /// The filesystem path of the SQLite database file. Used by the
    /// `POST /admin/api/debug/recover` endpoint to give the operator
    /// the exact path for manual repair commands.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Close and reopen BOTH connections (writer + reader). This is
    /// necessary after a VACUUM that changes the DB file structure,
    /// or after an offline DB repair — the long-lived connections
    /// hold stale page caches that reference pages that no longer
    /// exist in the rebuilt DB file.
    ///
    /// **BLOCKING**: takes BOTH locks (writer then reader). Must not
    /// be called while any query is in flight — the caller must hold
    /// the writer lock before calling this (or ensure no concurrent
    /// access by other means).
    ///
    /// After reopening, the new connections see the current state of
    /// the DB file on disk (fresh page cache, fresh schema, fresh
    /// prepared-statement cache).
    pub fn reopen(&self) -> Result<()> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE;

        // Reopen the writer. We hold the writer lock (the caller
        // must have acquired it) so this is safe.
        // SAFETY: we're replacing the Connection inside the Mutex.
        // The old Connection is dropped (closed) when we assign the
        // new one. rusqlite::Connection::drop closes the SQLite
        // handle.
        let new_writer = Connection::open_with_flags(&*self.path, flags).map_err(
            crate::error::map_db_error_ctx(format!("reopen writer {}", self.path.display())),
        )?;
        configure_connection(&new_writer)?;

        // Reopen the reader. We need to take the reader lock too.
        let new_reader = Connection::open_with_flags(&*self.path, flags).map_err(
            crate::error::map_db_error_ctx(format!("reopen reader {}", self.path.display())),
        )?;
        configure_connection(&new_reader)?;

        // Replace the connections. The old connections are dropped
        // (and their SQLite handles closed) when we assign the new
        // ones.
        *self.writer.lock() = new_writer;
        *self.reader.lock() = new_reader;

        tracing::info!("DbPool: reopened both connections (writer + reader)");
        Ok(())
    }

    /// Open an *additional* `Connection` to the same SQLite file.
    pub fn open_connection(&self) -> Result<Connection> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE;
        let conn = Connection::open_with_flags(self.path.as_ref(), flags).map_err(|e| {
            CoreError::Database {
                message: format!("open extra connection {}: {}", self.path.display(), e),
                source: Some(Box::new(e)),
            }
        })?;
        configure_connection(&conn)?;
        Ok(conn)
    }
}

/// Apply the standard pragmas required by spec §8/§9.
fn configure_connection(conn: &Connection) -> Result<()> {
    let _ = conn.pragma_update(None, "auto_vacuum", "INCREMENTAL");
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(crate::error::map_db_error)?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(crate::error::map_db_error)?;
    conn.pragma_update(None, "busy_timeout", 5000)
        .map_err(crate::error::map_db_error)?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(crate::error::map_db_error)?;
    conn.pragma_update(None, "wal_autocheckpoint", 1000)
        .map_err(crate::error::map_db_error)?;
    conn.pragma_update(None, "mmap_size", 8 * 1024 * 1024)
        .map_err(crate::error::map_db_error)?;
    conn.pragma_update(None, "cache_size", -2000)
        .map_err(crate::error::map_db_error)?;
    conn.pragma_update(None, "temp_store", "MEMORY")
        .map_err(crate::error::map_db_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_creates_file_and_sets_pragmas() {
        let dir = tempdir();
        let path = dir.join("test.db");
        let pool = DbPool::open(&path).expect("open");
        let conn = pool.writer();

        let journal: String = conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .expect("journal_mode");
        assert_eq!(journal.to_ascii_lowercase(), "wal");

        let fk: i64 = conn
            .pragma_query_value(None, "foreign_keys", |r| r.get(0))
            .expect("foreign_keys");
        assert_eq!(fk, 1);

        let busy: i64 = conn
            .pragma_query_value(None, "busy_timeout", |r| r.get(0))
            .expect("busy_timeout");
        assert_eq!(busy, 5000);
    }

    fn tempdir() -> std::path::PathBuf {
        let base = std::env::temp_dir();
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = base.join(format!("openproxy-db-test-{}-{}", pid, nanos));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        dir
    }

    #[test]
    fn try_writer_for_returns_none_when_lock_is_held() {
        let dir = tempdir();
        let path = dir.join("test.db");
        let pool = DbPool::open(&path).expect("open");

        let _guard = pool.writer();

        let start = std::time::Instant::now();
        let result = pool.try_writer_for(std::time::Duration::from_millis(50));
        let elapsed = start.elapsed();

        assert!(result.is_none(), "lock should not be acquirable while held");
        assert!(
            elapsed < std::time::Duration::from_millis(150),
            "try_writer_for waited {:?}; should have failed fast",
            elapsed
        );
    }

    #[test]
    fn try_writer_for_succeeds_when_lock_is_free() {
        let dir = tempdir();
        let path = dir.join("test.db");
        let pool = DbPool::open(&path).expect("open");

        let start = std::time::Instant::now();
        let guard = pool
            .try_writer_for(std::time::Duration::from_millis(100))
            .expect("lock should be available");
        let elapsed = start.elapsed();

        assert!(elapsed < std::time::Duration::from_millis(50));
        drop(guard);
    }
}
