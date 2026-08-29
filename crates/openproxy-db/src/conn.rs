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
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// Alias for the writer guard returned by [`DbPool::writer`].
pub type WriterGuard<'a> = parking_lot::MutexGuard<'a, Connection>;

/// Alias for the reader guard returned by [`DbPool::reader`].
pub type ReaderGuard<'a> = parking_lot::MutexGuard<'a, Connection>;

/// Alias for the owned writer guard returned by [`DbPool::writer_guard`].
pub type ArcWriterGuard = parking_lot::ArcMutexGuard<parking_lot::RawMutex, Connection>;

/// Alias for the owned reader guard returned by [`DbPool::reader_guard`].
pub type ArcReaderGuard = parking_lot::ArcMutexGuard<parking_lot::RawMutex, Connection>;

/// Connection pool holding one serialized writer and one serialized reader.
/// SQLite file-level locking + rusqlite's lack of `Sync` on `Connection` mean we
/// guard both with a Mutex. A future r2d2-based pool can swap in true reader
/// concurrency without changing the public API beyond return types.
#[derive(Clone)]
pub struct DbPool {
    writer: Arc<Mutex<Connection>>,
    readers: Arc<Vec<Arc<Mutex<Connection>>>>,
    next_reader: Arc<AtomicUsize>,
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

        configure_temp_dir(&writer, path);
        configure_connection(&writer)?;

        // Readers: open multiple handles on the same file to avoid mutex contention
        // on high-throughput API endpoints.
        let num_readers = 16;
        let mut readers = Vec::with_capacity(num_readers);
        for i in 0..num_readers {
            let reader = open_and_configure_reader(path, flags, i)?;
            readers.push(Arc::new(Mutex::new(reader)));
        }

        Ok(Self {
            writer: Arc::new(Mutex::new(writer)),
            readers: Arc::new(readers),
            next_reader: Arc::new(AtomicUsize::new(0)),
            path: Arc::from(path),
        })
    }

    #[inline]
    fn get_reader_idx(&self) -> usize {
        self.next_reader.fetch_add(1, Ordering::Relaxed) % self.readers.len()
    }

    /// Acquire the serialized writer. Blocks until the previous writer is released.
    pub fn writer(&self) -> WriterGuard<'_> {
        parking_lot::Mutex::lock(&self.writer)
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
        Arc::clone(&self.writer)
    }

    /// Acquire the serialized writer with an owned guard backed by Arc.
    pub fn writer_guard(&self) -> ArcWriterGuard {
        parking_lot::Mutex::lock_arc(&self.writer)
    }

    /// Acquire the serialized reader with an owned guard backed by Arc.
    pub fn reader_guard(&self) -> ArcReaderGuard {
        let idx = self.get_reader_idx();
        parking_lot::Mutex::lock_arc(&self.readers[idx])
    }

    /// Acquire the serialized reader. Blocks until the previous reader is released.
    pub fn reader(&self) -> ReaderGuard<'_> {
        let idx = self.get_reader_idx();
        parking_lot::Mutex::lock(&self.readers[idx])
    }

    /// Try to acquire the reader lock for at most `timeout` (blocking).
    /// Returns `None` if the lock could not be acquired in time. Used by
    /// analytics queries so a long-running reader doesn't block the
    /// admin endpoint indefinitely — the caller returns 503 and the
    /// operator can retry.
    pub fn try_reader_for(&self, timeout: std::time::Duration) -> Option<ReaderGuard<'_>> {
        let idx = self.get_reader_idx();
        self.readers[idx].try_lock_for(timeout)
    }

    /// Run a closure against the serialized writer connection.
    pub fn with_conn<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Connection) -> R,
    {
        let guard = parking_lot::Mutex::lock(&self.writer);
        f(&guard)
    }

    /// The filesystem path of the SQLite database file. Used by the
    /// Number of reader handles in the pool.
    pub fn reader_count(&self) -> usize {
        self.readers.len()
    }

    /// Access the underlying path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reopen ALL connections (writer + readers) against the database file.
    ///
    /// This closes every existing `rusqlite::Connection` and opens a fresh
    /// one in-place, preserving the same `DbPool` instance. This is
    /// necessary after a VACUUM that changes the DB file structure,
    /// or after an offline DB repair — the long-lived connections
    /// hold stale page caches that reference pages that no longer
    /// exist in the rebuilt DB file.
    ///
    /// **BLOCKING**: takes ALL locks (writer then readers). Must not
    /// be called while any query is in flight — the caller must hold
    /// the writer lock before calling this (or ensure no concurrent
    /// access by other means).
    ///
    /// After reopening, the new connections see the current state of
    /// the DB file on disk (fresh page cache, fresh schema, fresh
    /// prepared-statement cache).
    pub fn reopen(&self) -> Result<()> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE;

        let new_writer = Connection::open_with_flags(&*self.path, flags).map_err(
            crate::error::map_db_error_ctx(format!("reopen writer {}", self.path.display())),
        )?;
        configure_connection(&new_writer)?;

        let mut new_readers = Vec::with_capacity(self.readers.len());
        for i in 0..self.readers.len() {
            new_readers.push(reopen_and_configure_reader(&self.path, flags, i)?);
        }

        *parking_lot::Mutex::lock(&self.writer) = new_writer;
        for (i, new_r) in new_readers.into_iter().enumerate() {
            *parking_lot::Mutex::lock(&self.readers[i]) = new_r;
        }

        tracing::info!("DbPool: reopened all connections (writer + readers)");
        Ok(())
    }

    /// Open an *additional* `Connection` to the same SQLite file.
    pub fn open_connection(&self) -> Result<Connection> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE;
        let conn = Connection::open_with_flags(self.path.as_ref(), flags).map_err(|e| {
            CoreError::Database {
                message: format!("open extra connection {}: {}", self.path.display(), e),
                source: Some(std::sync::Arc::new(e)),
            }
        })?;
        configure_connection(&conn)?;
        Ok(conn)
    }
}

fn configure_temp_dir(conn: &Connection, path: &Path) {
    if let Some(parent) = path.parent() {
        let p_str = parent.to_string_lossy();
        if !p_str.is_empty() {
            let _ = conn.pragma_update(None, "temp_store_directory", &*p_str);
        }
    }
}

fn open_and_configure_reader(path: &Path, flags: OpenFlags, idx: usize) -> Result<Connection> {
    let reader = Connection::open_with_flags(path, flags).map_err(
        crate::error::map_db_error_ctx(format!("open reader {idx} for {}", path.display())),
    )?;
    configure_connection(&reader)?;
    Ok(reader)
}

fn reopen_and_configure_reader(path: &Path, flags: OpenFlags, idx: usize) -> Result<Connection> {
    let r = Connection::open_with_flags(path, flags).map_err(crate::error::map_db_error_ctx(
        format!("reopen reader {idx} for {}", path.display()),
    ))?;
    configure_connection(&r)?;
    Ok(r)
}

/// Apply the standard pragmas required by spec §8/§9.
fn configure_connection(conn: &Connection) -> Result<()> {
    let _ = conn.pragma_update(None, "auto_vacuum", "INCREMENTAL");
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    conn.execute_batch(
        "PRAGMA foreign_keys = ON; \
         PRAGMA busy_timeout = 5000; \
         PRAGMA synchronous = NORMAL; \
         PRAGMA wal_autocheckpoint = 1000; \
         PRAGMA mmap_size = 8388608; \
         PRAGMA cache_size = -2000; \
         PRAGMA temp_store = MEMORY;",
    )
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
            .map_or(0, |d| d.as_nanos());
        let dir = base.join(format!("openproxy-db-test-{pid}-{nanos}"));
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
            "try_writer_for waited {elapsed:?}; should have failed fast"
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
