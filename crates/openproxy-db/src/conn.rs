//! SQLite connection pool.
//!
//! One serialized writer and independently opened reader connections. Cloning
//! the pool shares ownership, not the readers' underlying SQLite handles.
//!
//! This avoids adding `r2d2` / `r2d2_sqlite` deps for the MVP. If we ever need
//! concurrent writers, swap the writer field for a real pool.

use openproxy_types::config::SqliteSynchronous;
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
    synchronous: SqliteSynchronous,
    /// Monotonic epoch bumped by the writer connection's commit hook on
    /// every COMMIT. Read-side caches (routing snapshot, account cache) key
    /// on (epoch) and rebuild when it moves — `PRAGMA data_version` does not
    /// observe same-process commits, so this counter is the correctness
    /// anchor for in-process writes.
    epoch: Arc<std::sync::atomic::AtomicU64>,
    /// Dedicated probe connection for `PRAGMA data_version`, the mirror
    /// signal of `epoch`: it moves ONLY on commits made by *other*
    /// connections/processes. Together the two classify any file change:
    /// epoch moved → internal, data_version moved → external, both → mixed.
    /// `None` when the pool has no on-disk path worth probing.
    probe: Option<Arc<Mutex<Connection>>>,
    _cleanup: Option<Arc<crate::testing::TempDir>>,
}

/// Routing-surface epochs bumped by the writer's update hook. `CONFIG`
/// covers `models`, `providers`, `combos`, `combo_targets`: a bump
/// invalidates the routing snapshot immediately. `COOLDOWN` covers
/// `target_cooldowns`: cooldown rows churn at request rate, so the snapshot
/// serves them from its stored timestamps and only rebuilds them on a short
/// debounce — a new cooldown becomes visible within ~50ms instead of
/// forcing a full 8k-model rebuild per failed request.
static ROUTING_CONFIG_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ROUTING_COOLDOWN_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn routing_config_epoch() -> u64 {
    ROUTING_CONFIG_EPOCH.load(std::sync::atomic::Ordering::Acquire)
}

pub fn routing_cooldown_epoch() -> u64 {
    ROUTING_COOLDOWN_EPOCH.load(std::sync::atomic::Ordering::Acquire)
}

pub(crate) fn bump_routing_config_epoch() {
    ROUTING_CONFIG_EPOCH.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
}

pub(crate) fn bump_routing_cooldown_epoch() {
    ROUTING_COOLDOWN_EPOCH.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
}

/// Writer-lock budget for hot-path operations.
///
/// Bounded at a 100ms ceiling so that long-running operations (e.g. heavy admin
/// queries or background compaction) cannot stall latency-critical requests.
/// In the modern architecture, high-throughput writes such as usage records
/// are admitted through the persistent FIFO usage journal with backpressure
/// rather than silently dropping usage data.
pub const HOT_PATH_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

/// Writer-lock budget for admin/dashboard queries. Longer than the hot path
/// because the operator explicitly asked for the result.
pub const ADMIN_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Reason a `try_lock` returned `None` instead of a guard. Used by callers
/// to log and record lock-acquisition timeouts for bounded backoff.
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
        Self::open_with_options(path, reader_count, SqliteSynchronous::Full)
    }

    /// Open (or create) the SQLite database at `path` with explicit reader count
    /// and `SqliteSynchronous` durability pragma policy.
    ///
    /// Durability note: `SqliteSynchronous::Full` issues an fsync on every
    /// transaction commit as well as on WAL checkpoints. However, durability
    /// ultimately depends on the underlying storage honoring fsync/flush commands;
    /// it does not grant absolute immunity to power loss.
    pub fn open_with_options(
        path: &Path,
        reader_count: usize,
        synchronous: SqliteSynchronous,
    ) -> Result<Self> {
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
        with_busy_retry("DbPool::open", || {
            Self::open_inner(path, readers, synchronous)
        })
        .inspect_err(|e| {
            tracing::error!(
                path = %path.display(),
                error = %e,
                "DbPool::open failed (including BUSY retries)",
            );
        })
    }

    /// Builds the pool. Public callers go through [`DbPool::open`] or [`DbPool::open_with_options`],
    /// which wrap this in `with_busy_retry`.
    fn open_inner(path: &Path, num_readers: usize, synchronous: SqliteSynchronous) -> Result<Self> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE;

        let writer = Connection::open_with_flags(path, flags).map_err(
            crate::error::map_db_error_ctx(format!("open {}", path.display())),
        )?;

        configure_temp_dir(&writer, path);
        configure_connection(&writer, true, synchronous)?;

        // Commit hook bumps the pool epoch so read-side caches can tell a
        // write happened without polling `PRAGMA data_version` (which only
        // sees external commits).
        let epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        install_cache_hooks(&writer, &epoch);

        // Extra handles on the same file keep mutex contention off the
        // high-throughput API endpoints.
        let mut readers = Vec::with_capacity(num_readers);
        for i in 0..num_readers {
            let reader = open_and_configure_reader(path, flags, i, synchronous)?;
            readers.push(Arc::new(Mutex::new(reader)));
        }

        // External-commit probe. If it cannot be opened the pool still
        // works; the caches just fall back to the mtime heuristic.
        let probe = open_and_configure_reader(path, flags, usize::MAX, synchronous)
            .ok()
            .map(|c| Arc::new(Mutex::new(c)));

        Ok(Self {
            writer: Arc::new(Mutex::new(writer)),
            readers: Arc::new(readers),
            next_reader: Arc::new(AtomicUsize::new(0)),
            path: Arc::from(path),
            synchronous,
            epoch,
            probe,
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
            synchronous: pool.synchronous,
            epoch: pool.epoch,
            probe: pool.probe,
            _cleanup: Some(temp_dir),
        })
    }

    #[inline]
    fn get_reader_idx(&self) -> usize {
        self.next_reader.fetch_add(1, Ordering::Relaxed) % self.readers.len()
    }

    /// Acquire the serialized writer. Blocks until the previous writer is released.
    pub fn writer(&self) -> WriterGuard<'_> {
        #[cfg(feature = "sql-profile")]
        let start = std::time::Instant::now();
        let guard = self.writer.lock();
        #[cfg(feature = "sql-profile")]
        crate::sql_profile::record_lock_wait(crate::sql_profile::LockKind::Writer, start.elapsed());
        guard
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
    ///
    /// NOTE: Locking directly on the returned `Arc<Mutex<Connection>>` bypasses
    /// `DbPool::writer()` and its `sql-profile` lock acquisition instrumentation.
    /// Pipeline components monitor and measure this lock wait separately.
    pub fn writer_arc(&self) -> Arc<Mutex<Connection>> {
        Arc::clone(&self.writer)
    }

    /// Acquire the serialized writer with an owned guard backed by Arc.
    pub fn writer_guard(&self) -> ArcWriterGuard {
        #[cfg(feature = "sql-profile")]
        let start = std::time::Instant::now();
        let guard = self.writer.lock_arc();
        #[cfg(feature = "sql-profile")]
        crate::sql_profile::record_lock_wait(crate::sql_profile::LockKind::Writer, start.elapsed());
        guard
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
        #[cfg(feature = "sql-profile")]
        let start = std::time::Instant::now();
        let n = self.readers.len();
        let mut spins = 0u32;
        loop {
            let start_idx = self.get_reader_idx();
            for offset in 0..n {
                let idx = (start_idx + offset) % n;
                if let Some(guard) = self.readers[idx].try_lock_arc() {
                    #[cfg(feature = "sql-profile")]
                    crate::sql_profile::record_lock_wait(
                        crate::sql_profile::LockKind::Reader,
                        start.elapsed(),
                    );
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
        #[cfg(feature = "sql-profile")]
        let start = std::time::Instant::now();
        let n = self.readers.len();
        let mut spins = 0u32;
        loop {
            let start_idx = self.get_reader_idx();
            for offset in 0..n {
                let idx = (start_idx + offset) % n;
                if let Some(guard) = self.readers[idx].try_lock() {
                    #[cfg(feature = "sql-profile")]
                    crate::sql_profile::record_lock_wait(
                        crate::sql_profile::LockKind::Reader,
                        start.elapsed(),
                    );
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

    /// Number of reader handles in the pool.
    pub fn reader_count(&self) -> usize {
        self.readers.len()
    }

    /// Returns the SQLite synchronous durability policy configured for this pool.
    pub fn synchronous(&self) -> SqliteSynchronous {
        self.synchronous
    }

    /// Access the underlying filesystem path of the SQLite database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Epoch bumped by the writer's commit hook. Cheap read-side cache
    /// invalidation: `load + compare` replaces a `PRAGMA data_version`
    /// round-trip and also sees commits made by this process.
    pub fn write_epoch(&self) -> u64 {
        self.epoch.load(std::sync::atomic::Ordering::Acquire)
    }

    /// External-writer probe: the modification times of the database file and
    /// its WAL. The writer's update/commit hooks only observe commits made
    /// through this pool — an `sqlite3` CLI session, a backup restore, or a
    /// second process writes the same file without touching our epochs. A
    /// pair of `stat` calls (~200ns) per request closes that hole: caches key
    /// on this value alongside the epochs and rebuild when either moves.
    ///
    /// WAL mode writes go to `<db>-wal`, not the main file, until a
    /// checkpoint — so both mtimes must be tracked. A missing WAL (fresh DB,
    /// just-checkpointed) reports 0 and is fine.
    pub fn file_version(&self) -> (u64, u64) {
        fn mtime_nanos(p: &std::path::Path) -> u64 {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos() as u64)
        }
        let mut wal_path = self.path.as_os_str().to_os_string();
        wal_path.push("-wal");
        (
            mtime_nanos(&self.path),
            mtime_nanos(std::path::Path::new(&wal_path)),
        )
    }

    /// `PRAGMA data_version` read on the dedicated probe connection: moves
    /// only when a commit lands through *another* connection or process.
    /// Returns `None` when the probe could not be opened — callers then
    /// treat any file change as potentially external (conservative rebuild).
    /// Costs one short lock + one cached PRAGMA query; call it only when the
    /// cheaper `file_version`/`write_epoch` pair is ambiguous, never per
    /// request unconditionally.
    pub fn external_version(&self) -> Option<u64> {
        self.probe
            .as_ref()
            .and_then(|p| p.try_lock())
            .and_then(|c| {
                c.pragma_query_value(None, "data_version", |r| r.get::<_, i64>(0))
                    .ok()
                    .map(|v| v.max(0) as u64)
            })
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
    /// Acquires each connection lock in sequence (writer then readers) to swap
    /// in the freshly opened handles; callers must NOT hold the writer lock
    /// or any reader locks when calling this to prevent deadlocks.
    pub fn reopen(&self) -> Result<()> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE;

        let new_writer = Connection::open_with_flags(&*self.path, flags).map_err(
            crate::error::map_db_error_ctx(format!("reopen writer {}", self.path.display())),
        )?;
        configure_connection(&new_writer, true, self.synchronous)?;
        install_cache_hooks(&new_writer, &self.epoch);

        let mut new_readers = Vec::with_capacity(self.readers.len());
        for (i, _) in self.readers.iter().enumerate() {
            new_readers.push(reopen_and_configure_reader(
                &self.path,
                flags,
                i,
                self.synchronous,
            )?);
        }

        *self.writer.lock() = new_writer;
        for (slot, new_r) in self.readers.iter().zip(new_readers) {
            *parking_lot::Mutex::lock(slot) = new_r;
        }
        // The probe connection is a separate handle on the same file; swap
        // it too so post-VACUUM probes keep working.
        if let Some(probe) = &self.probe
            && let Ok(new_probe) =
                open_and_configure_reader(&self.path, flags, usize::MAX, self.synchronous)
        {
            *probe.lock() = new_probe;
        }

        tracing::info!("DbPool: reopened all connections (writer + readers)");
        Ok(())
    }

    /// Open an *additional* `Connection` to the same SQLite file configured with the pool's policy.
    pub fn open_connection(&self) -> Result<Connection> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE;
        let conn = Connection::open_with_flags(self.path.as_ref(), flags).map_err(|e| {
            CoreError::Database {
                message: format!("open extra connection {}: {}", self.path.display(), e),
                source: Some(std::sync::Arc::new(e)),
            }
        })?;
        configure_connection(&conn, false, self.synchronous)?;
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

/// Install the commit + update hooks that keep read-side caches (routing
/// snapshot, account cache) in sync with the writer. Called at pool open
/// AND inside `reopen()` — a reopened `Connection` is a fresh SQLite handle
/// and loses every hook installed on the old one, so skipping this would
/// silently freeze the caches after a VACUUM/repair reopen.
fn install_cache_hooks(writer: &Connection, epoch: &Arc<std::sync::atomic::AtomicU64>) {
    {
        let epoch = Arc::clone(epoch);
        if let Err(e) = writer.commit_hook(Some(move || {
            epoch.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            false
        })) {
            debug_assert!(false, "commit_hook install failed: {e}");
        }
    }

    // Update hook bumps per-surface epochs by table name, covering every
    // write path — including raw `execute` calls that bypass the helper
    // functions. `accounts` feeds the pipeline's get_account cache; the
    // routing tables feed the in-memory routing snapshot. A rolled-back
    // transaction still bumps (harmless extra rebuild, never staleness).
    fn table_epoch_hook(_action: rusqlite::hooks::Action, _db: &str, table: &str, _rowid: i64) {
        match table {
            "accounts" => crate::accounts::bump_epoch(),
            "target_cooldowns" => bump_routing_cooldown_epoch(),
            "models" | "providers" | "combos" | "combo_targets" => bump_routing_config_epoch(),
            _ => {}
        }
    }
    if let Err(e) = writer.update_hook(Some(table_epoch_hook)) {
        debug_assert!(false, "update_hook install failed: {e}");
    }
}

fn open_and_configure_reader(
    path: &Path,
    flags: OpenFlags,
    idx: usize,
    synchronous: SqliteSynchronous,
) -> Result<Connection> {
    let reader = Connection::open_with_flags(path, flags).map_err(
        crate::error::map_db_error_ctx(format!("open reader {idx} for {}", path.display())),
    )?;
    configure_connection(&reader, false, synchronous)?;
    Ok(reader)
}

fn reopen_and_configure_reader(
    path: &Path,
    flags: OpenFlags,
    idx: usize,
    synchronous: SqliteSynchronous,
) -> Result<Connection> {
    let r = Connection::open_with_flags(path, flags).map_err(crate::error::map_db_error_ctx(
        format!("reopen reader {idx} for {}", path.display()),
    ))?;
    configure_connection(&r, false, synchronous)?;
    Ok(r)
}

/// Apply the standard pragmas required by spec §8/§9.
///
/// # Durability Note
/// In WAL mode, `PRAGMA synchronous = FULL` issues an fsync on each transaction
/// commit as well as on WAL checkpoints, significantly reducing the risk of
/// journal corruption or lost committed transactions during ungraceful power loss
/// or OS crashes. However, no software setting can guarantee absolute power-loss
/// immunity: durability is ultimately bounded by the underlying storage device
/// honoring fsync/flush commands (e.g. disk drive write caches).
/// `PRAGMA synchronous = NORMAL` skips fsync on regular WAL commits (syncing only
/// during checkpoints), providing higher throughput at the expense of potential
/// loss of the most recent transactions if power fails before a checkpoint completes.
fn configure_connection(
    conn: &Connection,
    is_writer: bool,
    synchronous: SqliteSynchronous,
) -> Result<()> {
    let _ = conn.pragma_update(None, "auto_vacuum", "INCREMENTAL");
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    let sync_val = synchronous.as_pragma_str();
    if is_writer {
        conn.execute_batch(&format!(
            "PRAGMA foreign_keys = ON; \
             PRAGMA busy_timeout = 5000; \
             PRAGMA synchronous = {sync_val}; \
             PRAGMA wal_autocheckpoint = 250; \
             PRAGMA mmap_size = 0; \
             PRAGMA cache_size = -512; \
             PRAGMA temp_store = FILE;",
        ))
    } else {
        conn.execute_batch(&format!(
            "PRAGMA foreign_keys = ON; \
             PRAGMA busy_timeout = 5000; \
             PRAGMA synchronous = {sync_val}; \
             PRAGMA wal_autocheckpoint = 250; \
             PRAGMA mmap_size = 0; \
             PRAGMA cache_size = -256; \
             PRAGMA temp_store = FILE;",
        ))
    }
    .map_err(crate::error::map_db_error)?;
    #[cfg(feature = "sql-profile")]
    crate::sql_profile::install(conn);
    Ok(())
}

#[cfg(test)]
mod tests;
