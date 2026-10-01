//! SQLite connection pool.
//!
//! One serialized writer and independently opened reader connections. Cloning
//! the pool shares ownership, not the readers' underlying SQLite handles.
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

use crate::error::with_busy_retry;

/// Alias for the writer guard returned by [`DbPool::writer`].
pub type WriterGuard<'a> = parking_lot::MutexGuard<'a, Connection>;

/// Alias for the reader guard returned by [`DbPool::reader`].
pub type ReaderGuard<'a> = parking_lot::MutexGuard<'a, Connection>;

/// Alias for the owned writer guard returned by [`DbPool::writer_guard`].
pub type ArcWriterGuard = parking_lot::ArcMutexGuard<parking_lot::RawMutex, Connection>;

/// Alias for the owned reader guard returned by [`DbPool::reader_guard`].
pub type ArcReaderGuard = parking_lot::ArcMutexGuard<parking_lot::RawMutex, Connection>;

/// One serialized writer and bounded independent readers, each mutex-protected
/// because rusqlite connections are Send but not Sync.
#[derive(Clone)]
pub struct DbPool {
    writer: Arc<Mutex<Connection>>,
    readers: Arc<Vec<Arc<Mutex<Connection>>>>,
    next_reader: Arc<AtomicUsize>,
    /// Path of the SQLite file. [`DbPool::open_connection`] needs it to open an
    /// extra owned `Connection`, since `Connection` is not `Clone` and a second
    /// handle requires opening the file again.
    path: Arc<Path>,
    _cleanup: Option<Arc<crate::testing::TempDir>>,
}

/// Writer-lock budget for hot-path inserts.
///
/// `cost::record` takes the writer on every chat request to persist a usage
/// row. A long admin query holding it (e.g. a 30-day usage summary over ~10k
/// rows) would block every concurrent chat request. At a 100ms ceiling the
/// worst case is one lost usage row (logged, returned as `None`), never a hung
/// client.
pub const HOT_PATH_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

/// Writer-lock budget for admin/dashboard queries. Longer than the hot path
/// because the operator explicitly asked for the result.
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
    /// Open (or create) the SQLite database at `path`, configure pragmas and
    /// return a ready pool. Run migrations on the writer before querying.
    ///
    /// `with_busy_retry` wraps the whole open path: when another process holds
    /// the DB file lock (e.g. a crash-restart loop where the previous instance
    /// never released the writer mutex), `Connection::open_with_flags` can
    /// surface `SQLITE_BUSY` once `busy_timeout` elapses. Retrying with
    /// 50ms+100ms backoff covers the handover window without making real
    /// failures noisy.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_readers(path, 0)
    }

    pub fn open_with_readers(path: &Path, reader_count: usize) -> Result<Self> {
        if reader_count > 32 {
            return Err(CoreError::Config(
                "SQLite reader count must be between 0 and 32".into(),
            ));
        }
        let readers = if reader_count == 0 {
            std::thread::available_parallelism()
                .map_or(2, std::num::NonZeroUsize::get)
                .clamp(2, 8)
        } else {
            reader_count
        };
        with_busy_retry("DbPool::open", || Self::open_inner(path, readers)).inspect_err(|e| {
            tracing::error!(
                path = %path.display(),
                error = %e,
                "DbPool::open failed (including BUSY retries)",
            );
        })
    }

    /// Builds the pool. Public callers go through [`DbPool::open`], which
    /// wraps this in `with_busy_retry`.
    fn open_inner(path: &Path, num_readers: usize) -> Result<Self> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE;

        let writer = Connection::open_with_flags(path, flags).map_err(
            crate::error::map_db_error_ctx(format!("open {}", path.display())),
        )?;

        configure_temp_dir(&writer, path);
        configure_connection(&writer, true)?;

        // Extra handles on the same file keep mutex contention off the
        // high-throughput API endpoints.
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
            _cleanup: None,
        })
    }

    /// Isolated test pool on a temporary directory with all migrations
    /// applied. The directory is cleaned up on drop.
    pub fn test_pool() -> Result<Self> {
        Self::test_pool_with_prefix("openproxy-test")
    }

    /// Isolated test pool under a custom directory prefix.
    pub fn test_pool_with_prefix(prefix: &str) -> Result<Self> {
        let temp_dir = Arc::new(
            crate::testing::TempDir::new(prefix)
                .map_err(|e| openproxy_types::error::CoreError::Internal(e.to_string()))?,
        );
        let db_path = temp_dir.path().join("test.db");
        let pool = Self::open(&db_path)?;
        {
            let mut w = pool.writer();
            crate::migrations::run(&mut w)?;
        }
        Ok(Self {
            writer: pool.writer,
            readers: pool.readers,
            next_reader: pool.next_reader,
            path: pool.path,
            _cleanup: Some(temp_dir),
        })
    }

    #[inline]
    fn get_reader_idx(&self) -> usize {
        self.next_reader.fetch_add(1, Ordering::Relaxed) % self.readers.len()
    }

    /// Acquire the serialized writer. Blocks until the previous writer is released.
    pub fn writer(&self) -> WriterGuard<'_> {
        self.writer.lock()
    }

    /// Try to acquire the writer lock for at most `timeout`, returning `None`
    /// on expiry and leaving the drop/retry/503 decision to the caller. Bounded
    /// this way, a long admin query holding the writer cannot freeze the hot
    /// path indefinitely.
    pub fn try_writer_for(&self, timeout: std::time::Duration) -> Option<WriterGuard<'_>> {
        self.writer.try_lock_for(timeout)
    }

    /// Clone the writer mutex's [`Arc`] handle, for long-lived consumers (e.g.
    /// [`crate::pipeline::Pipeline`]) that lock the connection repeatedly and
    /// move the handle into spawned tasks. Every `lock()` still serializes.
    pub fn writer_arc(&self) -> Arc<Mutex<Connection>> {
        Arc::clone(&self.writer)
    }

    /// Acquire the serialized writer with an owned guard backed by Arc.
    pub fn writer_guard(&self) -> ArcWriterGuard {
        self.writer.lock_arc()
    }

    #[inline]
    fn reader_backoff(spins: &mut u32, max_sleep: Option<std::time::Duration>) {
        if *spins < 8 {
            for _ in 0..(1 << *spins) {
                std::hint::spin_loop();
            }
        } else if *spins < 16 {
            std::thread::yield_now();
        } else {
            let micros = (50 * (1 << (*spins - 16).min(4))).min(500);
            let mut dur = std::time::Duration::from_micros(micros);
            if let Some(max) = max_sleep {
                dur = dur.min(max);
            }
            std::thread::sleep(dur);
        }
        *spins = spins.saturating_add(1);
    }

    /// Acquire the serialized reader with an owned guard backed by Arc. Scans
    /// readers non-blockingly with adaptive backoff, so a busy reader cannot
    /// cause Head-of-Line blocking or a deadlock.
    pub fn reader_guard(&self) -> ArcReaderGuard {
        let n = self.readers.len();
        let mut spins = 0u32;
        loop {
            let start = self.get_reader_idx();
            for offset in 0..n {
                let idx = (start + offset) % n;
                if let Some(guard) = self.readers[idx].try_lock_arc() {
                    return guard;
                }
            }
            Self::reader_backoff(&mut spins, None);
        }
    }

    /// Acquire the serialized reader. Blocks until a reader is released.
    /// Performs an opportunistic non-blocking scan across readers with adaptive
    /// backoff to prevent Head-of-Line blocking or deadlocks on busy readers.
    pub fn reader(&self) -> ReaderGuard<'_> {
        let n = self.readers.len();
        let mut spins = 0u32;
        loop {
            let start = self.get_reader_idx();
            for offset in 0..n {
                let idx = (start + offset) % n;
                if let Some(guard) = self.readers[idx].try_lock() {
                    return guard;
                }
            }
            Self::reader_backoff(&mut spins, None);
        }
    }

    /// Try to acquire the reader lock for at most `timeout` (blocking).
    /// Returns `None` if the lock could not be acquired in time — the
    /// caller decides what to do. Performs an opportunistic non-blocking
    /// scan across all readers with adaptive backoff until acquired or timed out.
    pub fn try_reader_for(&self, timeout: std::time::Duration) -> Option<ReaderGuard<'_>> {
        let n = self.readers.len();
        let deadline = std::time::Instant::now() + timeout;
        let mut spins = 0u32;
        loop {
            let start = self.get_reader_idx();
            for offset in 0..n {
                let idx = (start + offset) % n;
                if let Some(guard) = self.readers[idx].try_lock() {
                    return Some(guard);
                }
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return None;
            }
            Self::reader_backoff(&mut spins, Some(deadline - now));
        }
    }

    /// Run a closure against the serialized writer connection.
    pub fn with_conn<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Connection) -> R,
    {
        let guard = self.writer.lock();
        f(&guard)
    }

    /// Execute a read-only closure on a background thread via `tokio::task::spawn_blocking`,
    /// acquiring a reader guard within that thread. Prevents retaining locks across `.await` points.
    pub async fn spawn_read<F, R>(&self, f: F) -> std::result::Result<R, CoreError>
    where
        F: FnOnce(&Connection) -> std::result::Result<R, CoreError> + Send + 'static,
        R: Send + 'static,
    {
        let pool = self.clone();
        tokio::task::spawn_blocking(move || {
            let guard = pool.reader();
            f(&guard)
        })
        .await
        .map_err(|e| CoreError::Internal(format!("spawn_read join error: {e}")))?
    }

    /// Execute a write closure on a background thread via `tokio::task::spawn_blocking`,
    /// acquiring the serialized writer guard within that thread. Prevents retaining locks across `.await` points.
    pub async fn spawn_write<F, R>(&self, f: F) -> std::result::Result<R, CoreError>
    where
        F: FnOnce(&mut Connection) -> std::result::Result<R, CoreError> + Send + 'static,
        R: Send + 'static,
    {
        let pool = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = pool.writer();
            f(&mut guard)
        })
        .await
        .map_err(|e| CoreError::Internal(format!("spawn_write join error: {e}")))?
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
    /// Closes every existing `rusqlite::Connection` and opens a fresh one
    /// in place, keeping the same `DbPool`. Needed after a VACUUM that changes
    /// the file structure, or after an offline repair: the long-lived
    /// connections hold page caches referencing pages the rebuilt file no
    /// longer has. The new connections see the current on-disk state (fresh
    /// page cache, schema and prepared-statement cache).
    ///
    /// Takes every lock, writer then readers, so it must not run while a
    /// query is in flight: the caller holds the writer lock across this call.
    pub fn reopen(&self) -> Result<()> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE;

        let new_writer = Connection::open_with_flags(&*self.path, flags).map_err(
            crate::error::map_db_error_ctx(format!("reopen writer {}", self.path.display())),
        )?;
        configure_connection(&new_writer, true)?;

        let mut new_readers = Vec::with_capacity(self.readers.len());
        for (i, _) in self.readers.iter().enumerate() {
            new_readers.push(reopen_and_configure_reader(&self.path, flags, i)?);
        }

        *self.writer.lock() = new_writer;
        for (slot, new_r) in self.readers.iter().zip(new_readers) {
            *parking_lot::Mutex::lock(slot) = new_r;
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
        configure_connection(&conn, false)?;
        Ok(conn)
    }

    /// Run PRAGMA shrink_memory on writer and all readers to release unneeded heap pages.
    pub fn shrink_memory(&self) {
        if let Some(w) = self.try_writer_for(std::time::Duration::from_millis(100)) {
            let _ = w.execute_batch("PRAGMA shrink_memory;");
        }
        for r_mutex in self.readers.as_ref() {
            if let Some(r) = r_mutex.try_lock_for(std::time::Duration::from_millis(50)) {
                let _ = r.execute_batch("PRAGMA shrink_memory;");
            }
        }
    }

    /// Run PRAGMA wal_checkpoint(TRUNCATE) on writer.
    pub fn checkpoint_wal(&self) {
        if let Some(w) = self.try_writer_for(std::time::Duration::from_millis(500)) {
            let _ = w.pragma_update(None, "wal_checkpoint", "TRUNCATE");
        }
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
    configure_connection(&reader, false)?;
    Ok(reader)
}

fn reopen_and_configure_reader(path: &Path, flags: OpenFlags, idx: usize) -> Result<Connection> {
    let r = Connection::open_with_flags(path, flags).map_err(crate::error::map_db_error_ctx(
        format!("reopen reader {idx} for {}", path.display()),
    ))?;
    configure_connection(&r, false)?;
    Ok(r)
}

/// Apply the standard pragmas required by spec §8/§9.
fn configure_connection(conn: &Connection, is_writer: bool) -> Result<()> {
    let _ = conn.pragma_update(None, "auto_vacuum", "INCREMENTAL");
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    if is_writer {
        conn.execute_batch(
            "PRAGMA foreign_keys = ON; \
             PRAGMA busy_timeout = 5000; \
             PRAGMA synchronous = NORMAL; \
             PRAGMA wal_autocheckpoint = 250; \
             PRAGMA mmap_size = 0; \
             PRAGMA cache_size = -512; \
             PRAGMA temp_store = FILE;",
        )
    } else {
        conn.execute_batch(
            "PRAGMA foreign_keys = ON; \
             PRAGMA busy_timeout = 5000; \
             PRAGMA synchronous = NORMAL; \
             PRAGMA wal_autocheckpoint = 250; \
             PRAGMA mmap_size = 0; \
             PRAGMA cache_size = -256; \
             PRAGMA temp_store = FILE;",
        )
    }
    .map_err(crate::error::map_db_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_count_is_explicit_and_bounded() {
        let directory = crate::testing::TempDir::new("reader-count").unwrap();
        let path = directory.path().join("db.sqlite");
        let pool = DbPool::open_with_readers(&path, 4).unwrap();
        assert_eq!(pool.reader_count(), 4);
        assert!(DbPool::open_with_readers(&path, 33).is_err());
    }

    #[test]
    fn open_creates_file_and_sets_pragmas() {
        let pool = DbPool::test_pool().expect("test pool");
        assert!((2..=8).contains(&pool.reader_count()));
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

        let mmap: i64 = conn
            .pragma_query_value(None, "mmap_size", |r| r.get(0))
            .expect("mmap_size");
        assert_eq!(mmap, 0);

        let wal_autocheckpoint: i64 = conn
            .pragma_query_value(None, "wal_autocheckpoint", |r| r.get(0))
            .expect("wal_autocheckpoint");
        assert_eq!(wal_autocheckpoint, 250);

        let writer_cache: i64 = conn
            .pragma_query_value(None, "cache_size", |r| r.get(0))
            .expect("cache_size");
        assert_eq!(writer_cache, -512);

        let temp_store: i64 = conn
            .pragma_query_value(None, "temp_store", |r| r.get(0))
            .expect("temp_store");
        assert_eq!(temp_store, 1); // 1 = FILE

        drop(conn);

        let reader = pool.reader();
        let reader_cache: i64 = reader
            .pragma_query_value(None, "cache_size", |r| r.get(0))
            .expect("reader cache_size");
        assert_eq!(reader_cache, -256);

        let reader_mmap: i64 = reader
            .pragma_query_value(None, "mmap_size", |r| r.get(0))
            .expect("reader mmap_size");
        assert_eq!(reader_mmap, 0);
    }

    #[test]
    fn try_writer_for_returns_none_when_lock_is_held() {
        let pool = DbPool::test_pool().expect("test pool");

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
        let pool = DbPool::test_pool().expect("test pool");

        let start = std::time::Instant::now();
        let guard = pool
            .try_writer_for(std::time::Duration::from_millis(100))
            .expect("lock should be available");
        let elapsed = start.elapsed();

        assert!(elapsed < std::time::Duration::from_millis(50));
        drop(guard);
    }

    #[tokio::test]
    async fn test_spawn_read_and_spawn_write() {
        let pool = DbPool::test_pool().expect("test pool");

        // Write via spawn_write
        pool.spawn_write(|conn| {
            conn.execute(
                "CREATE TABLE IF NOT EXISTS test_spawn (id INTEGER PRIMARY KEY, val TEXT)",
                [],
            )
            .map_err(|e| CoreError::Database {
                message: e.to_string(),
                source: None,
            })?;
            conn.execute("INSERT INTO test_spawn (id, val) VALUES (1, 'hello')", [])
                .map_err(|e| CoreError::Database {
                    message: e.to_string(),
                    source: None,
                })?;
            Ok(())
        })
        .await
        .expect("spawn_write");

        // Read via spawn_read
        let val: String = pool
            .spawn_read(|conn| {
                conn.query_row("SELECT val FROM test_spawn WHERE id = 1", [], |row| {
                    row.get(0)
                })
                .map_err(|e| CoreError::Database {
                    message: e.to_string(),
                    source: None,
                })
            })
            .await
            .expect("spawn_read");

        assert_eq!(val, "hello");
    }

    #[test]
    fn reader_opportunistically_bypasses_held_reader_without_blocking() {
        let pool = DbPool::test_pool().expect("test pool");
        assert!(pool.readers.len() >= 2, "pool must have multiple readers");

        // Force next_reader to index 0
        pool.next_reader.store(0, Ordering::Relaxed);

        // Lock reader 0 directly to simulate a long-running query
        let _held_guard = pool.readers[0].lock();

        let start = std::time::Instant::now();
        // reader() starts scanning at index 0, skips reader 0 because try_lock fails,
        // and opportunistically acquires reader 1 without blocking.
        let acquired = pool.reader();
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "reader() took {elapsed:?}; should have bypassed reader 0 immediately"
        );

        let val: i64 = acquired
            .query_row("SELECT 42", [], |row| row.get(0))
            .expect("query_row on reader");
        assert_eq!(val, 42);
        drop(acquired);

        // Also test reader_guard bypasses reader 0
        pool.next_reader.store(0, Ordering::Relaxed);
        let guard = pool.reader_guard();
        let val_guard: i64 = guard
            .query_row("SELECT 84", [], |row| row.get(0))
            .expect("query_row on reader_guard");
        assert_eq!(val_guard, 84);
    }

    #[test]
    fn reader_contention_with_one_held_reader() {
        let pool = std::sync::Arc::new(DbPool::test_pool().expect("test pool"));
        assert!(pool.readers.len() >= 2);
        // Lock reader 0 indefinitely
        let _held = pool.readers[0].lock();

        let (tx, rx) = std::sync::mpsc::channel();

        let pool_clone = std::sync::Arc::clone(&pool);
        std::thread::spawn(move || {
            let mut handles = Vec::new();
            for _ in 0..8 {
                let pool = std::sync::Arc::clone(&pool_clone);
                handles.push(std::thread::spawn(move || {
                    for _ in 0..50 {
                        let guard = pool.reader();
                        std::thread::sleep(std::time::Duration::from_millis(1));
                        let val: i64 = guard.query_row("SELECT 1", [], |r| r.get(0)).unwrap();
                        assert_eq!(val, 1);
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
            let _ = tx.send(());
        });

        // If it deadlocks/hangs, timeout after 3 seconds
        let res = rx.recv_timeout(std::time::Duration::from_secs(3));
        assert!(
            res.is_ok(),
            "DEADLOCK DETECTED: reader acquisition blocked on locked reader 0!"
        );
    }

    #[test]
    fn try_reader_for_times_out_when_all_readers_locked() {
        let pool = DbPool::test_pool().expect("test pool");

        // Lock all readers
        let _guards: Vec<_> = pool.readers.iter().map(|r| r.lock()).collect();

        let start = std::time::Instant::now();
        let result = pool.try_reader_for(std::time::Duration::from_millis(50));
        let elapsed = start.elapsed();

        assert!(
            result.is_none(),
            "try_reader_for must return None when all readers are locked"
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(40),
            "try_reader_for must wait for the specified timeout: elapsed {elapsed:?}"
        );
    }

    #[test]
    fn try_reader_for_acquires_freed_reader_when_other_readers_held() {
        let pool = std::sync::Arc::new(DbPool::test_pool().expect("test pool"));
        pool.next_reader.store(0, Ordering::Relaxed);

        // Keep all but one reader locked in this thread. The releaser acquires
        // and drops its own guard; guards deliberately cannot migrate threads.
        let held: Vec<_> = pool
            .readers
            .iter()
            .skip(1)
            .map(|reader| reader.lock())
            .collect();
        let release_pool = Arc::clone(&pool);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();

        // Background thread releases reader 1 after 15ms
        let release_thread = std::thread::spawn(move || {
            let guard = release_pool.readers[0].lock();
            ready_tx.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(15));
            drop(guard);
        });
        ready_rx.recv().unwrap();

        // Calling try_reader_for with timeout of 60ms starting at index 0
        // Reader 1 becomes free at 15ms, well before 60ms timeout!
        let acquired = pool.try_reader_for(std::time::Duration::from_millis(60));
        release_thread.join().unwrap();

        assert!(
            acquired.is_some(),
            "try_reader_for should acquire reader 1 once freed before timeout!"
        );
        drop(held);
    }
}
