//! Cross-process exclusion for one prepaid key.
//!
//! The in-process lane (`tokio::sync::Mutex`) only serializes requests of one
//! server process. Two processes sharing a database (rolling restart, blue/green,
//! a manually started second instance) each read the same balance and would both
//! admit a spend the ledger cannot cover. This module adds the missing half: an
//! advisory lock on a per-key file next to the SQLite database, held from
//! admission until the request settles.
//!
//! Properties that matter:
//!
//! - **Release is RAII and crash-safe.** The lock lives on an open file handle;
//!   cancelling the request drops it, and the OS drops it when the process dies.
//!   There is no lease to expire, no stale marker to clean and nothing an
//!   operator ever has to unlock.
//! - **The lock file is never unlinked.** Removing it would let the next opener
//!   create a new inode and lock a file nobody else is watching, silently
//!   splitting the lock. The file is created once and reused forever.
//! - **Fail closed.** A path that cannot be canonicalized, a directory that
//!   cannot be created or an unsupported lock operation denies the request
//!   instead of admitting against an unserialized balance.
//! - **Never blocks the async runtime.** Waiting happens in `spawn_blocking`
//!   with a cancellation flag, so an aborted request stops waiting within one
//!   poll interval and cannot leave a blocking thread holding a lock nobody will
//!   ever drop: a cancelled wait has its result dropped, which closes the file
//!   handle and releases the lock even in the tiny window where the lock was won
//!   after the cancellation.

use openproxy_types::error::CoreError;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How long a waiting admission sleeps between lock attempts.
const LOCK_POLL: Duration = Duration::from_millis(20);
/// Directory suffix next to the database file.
const LOCK_DIR_SUFFIX: &str = "credit-locks";

/// One key's interprocess turn. Dropping it releases the advisory lock.
pub(crate) struct InterprocessLease {
    /// Held for its lifetime only: closing the handle releases the OS lock.
    _file: File,
    path: PathBuf,
}

impl InterprocessLease {
    /// Path of the lock file this lease holds. Diagnostics and tests only.
    #[cfg(all(test, feature = "payments"))]
    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Debug for InterprocessLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InterprocessLease")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// Wait for this key's cross-process turn.
///
/// # Errors
/// Fails closed when the database path cannot be resolved into a lock file, the
/// lock file cannot be opened, or the platform rejects the lock operation. A
/// dropped future abandons the wait without leaving the lock held.
pub(crate) async fn acquire(
    db_path: PathBuf,
    key_id: i64,
) -> openproxy_types::Result<InterprocessLease> {
    let cancel = Arc::new(AtomicBool::new(false));
    let hangup = CancelOnDrop(Arc::clone(&cancel));
    let task = tokio::task::spawn_blocking(move || acquire_blocking(&db_path, key_id, &cancel));
    let joined = task.await;
    // Releases the cancellation flag whether the task finished or was abandoned.
    drop(hangup);
    match joined {
        Ok(result) => result,
        Err(error) => Err(CoreError::Internal(format!(
            "credit key lock task failed to run: {error}"
        ))),
    }
}

/// Sets the cancellation flag when the waiting future is dropped, so the
/// blocking task stops polling instead of waiting for a lock nobody wants.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn acquire_blocking(
    db_path: &Path,
    key_id: i64,
    cancel: &AtomicBool,
) -> openproxy_types::Result<InterprocessLease> {
    let path = lock_file_path(db_path, key_id)?;
    // `truncate(false)` + `create(true)`: reuse the same inode forever.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| {
            CoreError::Internal(format!(
                "credit key lock {} could not be opened: {error}",
                path.display()
            ))
        })?;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(CoreError::Internal(
                "credit key lock: admission was cancelled while waiting".into(),
            ));
        }
        match file.try_lock() {
            Ok(()) => return Ok(InterprocessLease { _file: file, path }),
            Err(TryLockError::WouldBlock) => std::thread::sleep(LOCK_POLL),
            Err(TryLockError::Error(error)) => {
                return Err(CoreError::Internal(format!(
                    "credit key lock {} is unsupported here: {error}",
                    path.display()
                )));
            }
        }
    }
}

/// `<db dir>/.<db file>.credit-locks/key-<id>.lock`.
///
/// Derived from the canonical database file, so two pools opened on the same
/// file — regardless of relative paths or symlinks — meet in the same lock
/// directory, and nothing is ever placed in the system temp directory.
fn lock_file_path(db_path: &Path, key_id: i64) -> openproxy_types::Result<PathBuf> {
    let canonical = std::fs::canonicalize(db_path).map_err(|error| {
        CoreError::Internal(format!(
            "credit key lock: database {} cannot be resolved: {error}",
            db_path.display()
        ))
    })?;
    let parent = canonical.parent().ok_or_else(|| {
        CoreError::Internal(format!(
            "credit key lock: database {} has no parent directory",
            canonical.display()
        ))
    })?;
    let name: String = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("openproxy.db")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let dir = parent.join(format!(".{name}.{LOCK_DIR_SUFFIX}"));
    std::fs::create_dir_all(&dir).map_err(|error| {
        CoreError::Internal(format!(
            "credit key lock directory {} could not be created: {error}",
            dir.display()
        ))
    })?;
    Ok(dir.join(format!("key-{key_id}.lock")))
}

impl super::CreditsAdmissionGate {
    /// Take the key's in-process lane *and* its cross-process file lock. Both
    /// are released by the returned guard (RAII), including on cancellation and
    /// on a panic unwinding through the request.
    ///
    /// # Errors
    /// Fails closed when the interprocess lock cannot be established; the lane
    /// is released before returning.
    pub(crate) async fn acquire_leased(
        &self,
        db_id: usize,
        db_path: &Path,
        key_id: i64,
    ) -> openproxy_types::Result<super::CreditsLaneGuard> {
        let lane = self.acquire(db_id, key_id).await;
        let lease = acquire(db_path.to_path_buf(), key_id).await?;
        Ok(lane.with_lease(lease))
    }
}

/// Blocking acquisition used by the regression test that runs a real child
/// process. Test-only: never waits for cancellation.
#[cfg(all(test, feature = "payments"))]
pub(crate) fn acquire_blocking_for_test(
    db_path: &Path,
    key_id: i64,
) -> openproxy_types::Result<InterprocessLease> {
    acquire_blocking(db_path, key_id, &AtomicBool::new(false))
}

/// Lock file path for assertions. Test-only.
#[cfg(all(test, feature = "payments"))]
pub(crate) fn lock_file_path_for_test(
    db_path: &Path,
    key_id: i64,
) -> openproxy_types::Result<PathBuf> {
    lock_file_path(db_path, key_id)
}
