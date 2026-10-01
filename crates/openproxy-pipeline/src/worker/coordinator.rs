//! Journal admission coordinator for usage tracking backpressure.
//!
//! Provides lost-wakeup-free coordination between usage producers and the
//! background journal drain worker when SQLite pending usage reaches capacity.

use openproxy_types::error::{CoreError, Result};
use parking_lot::Mutex;
use rusqlite::Connection;
use std::collections::HashMap;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Weak};
use tokio::sync::{Notify, mpsc};

/// Coordinates backpressure between usage producers and the background journal drainer.
pub struct JournalCoordinator {
    drain_notify: Notify,
    acked_total: AtomicU64,
    #[cfg(test)]
    waiter_entered: Notify,
    #[cfg(test)]
    waiting_count: AtomicUsize,
}

impl Default for JournalCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl JournalCoordinator {
    pub fn new() -> Self {
        Self {
            drain_notify: Notify::new(),
            acked_total: AtomicU64::new(0),
            #[cfg(test)]
            waiter_entered: Notify::new(),
            #[cfg(test)]
            waiting_count: AtomicUsize::new(0),
        }
    }

    /// Registers interest in a drain signal before attempting admission.
    ///
    /// By instantiating `Notified` before calling `append`, race conditions
    /// with worker ACKs are eliminated: any ACK occurring while `append` is
    /// running or being evaluated in `spawn_blocking` is captured by Tokio's
    /// `Notify`, allowing `.await` to resolve immediately without lost wakeups.
    pub fn subscribe(&self) -> tokio::sync::futures::Notified<'_> {
        self.drain_notify.notified()
    }

    /// Notifies all registered waiters that one or more journal jobs have been
    /// acknowledged and drained from the database.
    pub fn notify_drained(&self, count: usize) {
        if count > 0 {
            self.acked_total.fetch_add(count as u64, Ordering::Relaxed);
            self.drain_notify.notify_waiters();
        }
    }

    /// Wakes all registered waiters on worker shutdown or cancellation
    /// without recording a fictitious ACK count.
    pub fn notify_closed(&self) {
        self.drain_notify.notify_waiters();
    }

    pub fn acked_total(&self) -> u64 {
        self.acked_total.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn on_capacity_wait(&self) {
        self.waiting_count.fetch_add(1, Ordering::Relaxed);
        self.waiter_entered.notify_waiters();
    }

    /// Asynchronously awaits until at least one producer encounters capacity exhaustion
    /// and enters the waiting state. Used for deterministic testing without spin-polling.
    #[cfg(test)]
    pub(crate) async fn await_waiter(&self) {
        if self.waiting_count.load(Ordering::Relaxed) > 0 {
            return;
        }
        let wait = self.waiter_entered.notified();
        if self.waiting_count.load(Ordering::Relaxed) > 0 {
            return;
        }
        wait.await;
    }
}

struct CoordinatorEntry {
    conn: Weak<Mutex<Connection>>,
    coordinator: Weak<JournalCoordinator>,
}

static COORDINATORS: LazyLock<Mutex<HashMap<usize, CoordinatorEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Resolves or registers a canonical `JournalCoordinator` for the given connection.
///
/// Prevents pointer-address ABA by verifying `Arc::ptr_eq` on the upgraded `Weak<Connection>`,
/// and automatically prunes expired weak references on access to prevent memory leaks.
pub fn coordinator_for(conn: &Arc<Mutex<Connection>>) -> Arc<JournalCoordinator> {
    let key = Arc::as_ptr(conn) as usize;
    let mut map = COORDINATORS.lock();
    map.retain(|_, entry| entry.conn.strong_count() > 0 && entry.coordinator.strong_count() > 0);
    if let Some(entry) = map.get(&key)
        && let Some(live_conn) = entry.conn.upgrade()
        && Arc::ptr_eq(&live_conn, conn)
        && let Some(strong) = entry.coordinator.upgrade()
    {
        return strong;
    }
    let coord = Arc::new(JournalCoordinator::new());
    map.insert(
        key,
        CoordinatorEntry {
            conn: Arc::downgrade(conn),
            coordinator: Arc::downgrade(&coord),
        },
    );
    coord
}

/// Registers an existing coordinator for the connection, enforcing a single canonical coordinator.
///
/// If an active coordinator is already registered for this connection, returns an error
/// if it does not match the provided coordinator to prevent silent desynchronization and hangs.
pub fn register_coordinator_for(
    conn: &Arc<Mutex<Connection>>,
    coordinator: Arc<JournalCoordinator>,
) -> Result<Arc<JournalCoordinator>> {
    let key = Arc::as_ptr(conn) as usize;
    let mut map = COORDINATORS.lock();
    map.retain(|_, entry| entry.conn.strong_count() > 0 && entry.coordinator.strong_count() > 0);
    if let Some(entry) = map.get(&key)
        && let Some(live_conn) = entry.conn.upgrade()
        && Arc::ptr_eq(&live_conn, conn)
        && let Some(existing) = entry.coordinator.upgrade()
    {
        if Arc::ptr_eq(&existing, &coordinator) {
            return Ok(existing);
        }
        return Err(CoreError::Internal(
            "conflicting coordinator already registered for this connection".into(),
        ));
    }
    map.insert(
        key,
        CoordinatorEntry {
            conn: Arc::downgrade(conn),
            coordinator: Arc::downgrade(&coordinator),
        },
    );
    Ok(coordinator)
}

/// Enqueues a usage job into the durable journal with bounded admission and backpressure.
///
/// Bounded admission (max 1024 concurrent producers) is enforced by reserving channel
/// capacity BEFORE any SQLite append attempt.
/// If SQLite journal depth is at capacity (100k prod), this function waits for effective
/// worker ACKs without polling or write/sleep loops.
/// If SQLite reports a non-capacity database error (such as disk full), it errors immediately.
pub async fn enqueue_with_backpressure(
    conn: &Arc<Mutex<Connection>>,
    background_tx: &mpsc::Sender<super::BackgroundJob>,
    job: super::BackgroundJob,
    coordinator: Option<&JournalCoordinator>,
) -> Result<()> {
    // 1. Reserve MPSC capacity BEFORE append to respect bounded 1024 concurrent admission limit
    let permit = background_tx.reserve().await.map_err(|_| {
        CoreError::Internal("usage worker is closed; record was not accepted".into())
    })?;

    let default_coord;
    let coord = match coordinator {
        Some(c) => c,
        None => {
            default_coord = coordinator_for(conn);
            &default_coord
        }
    };

    let payload = serde_json::to_string(&job)
        .map_err(|error| CoreError::Internal(format!("usage serialization failed: {error}")))?;

    loop {
        if background_tx.is_closed() {
            return Err(CoreError::Internal(
                "usage worker is closed; record was not accepted".into(),
            ));
        }

        // Register interest BEFORE attempting admission to eliminate lost-wakeup race.
        let drain_wait = coord.subscribe();

        let conn_clone = Arc::clone(conn);
        let payload_clone = payload.clone();

        let admission_result = tokio::task::spawn_blocking(move || {
            openproxy_db::usage_journal::append(&mut conn_clone.lock(), &payload_clone)
        })
        .await
        .map_err(|error| CoreError::Internal(format!("usage admission join failed: {error}")))?;

        match admission_result {
            Ok(_id) => {
                permit.send(super::BackgroundJob::JournalWake);
                return Ok(());
            }
            Err(ref err) if err.is_journal_capacity_exhausted() => {
                #[cfg(test)]
                coord.on_capacity_wait();
                tokio::select! {
                    biased;
                    () = background_tx.closed() => {
                        return Err(CoreError::Internal(
                            "usage worker closed while journal at capacity".into(),
                        ));
                    }
                    () = drain_wait => {
                        continue;
                    }
                }
            }
            Err(err) => {
                // Real DB error (e.g. disk full, corruption) - explicit error, no blind retry
                return Err(err);
            }
        }
    }
}
