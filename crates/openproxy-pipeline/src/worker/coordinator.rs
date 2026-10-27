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
    admission: Arc<Mutex<AdmissionQueue>>,
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
            admission: Arc::default(),
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

    /// Durably appends `payload` to the journal, sharing one transaction (and
    /// one fsync) with every producer that arrives while a commit is running.
    ///
    /// Each caller still resolves only after its own row is committed, so the
    /// durability contract of a single `append` is unchanged. The first
    /// producer of an idle queue becomes the leader and drains the queue on the
    /// blocking pool until it is empty.
    pub async fn admit(&self, conn: &Arc<Mutex<Connection>>, payload: String) -> Result<i64> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let lead = {
            let mut queue = self.admission.lock();
            queue.pending.push(Admission {
                conn: Arc::clone(conn),
                payload,
                reply: tx,
                #[cfg(feature = "benchmark-profile")]
                enqueued_at: std::time::Instant::now(),
            });
            !std::mem::replace(&mut queue.leader_active, true)
        };
        if lead {
            let mut leader = AdmissionLeader {
                queue: Arc::clone(&self.admission),
                clean_exit: false,
            };
            tokio::task::spawn_blocking(move || leader.drain());
        }
        rx.await
            .map_err(|_| CoreError::Internal("usage admission leader dropped the request".into()))?
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

struct Admission {
    conn: Arc<Mutex<Connection>>,
    payload: String,
    reply: tokio::sync::oneshot::Sender<Result<i64>>,
    #[cfg(feature = "benchmark-profile")]
    enqueued_at: std::time::Instant,
}

#[derive(Default)]
struct AdmissionQueue {
    pending: Vec<Admission>,
    leader_active: bool,
}

/// Largest group committed at once; bounds the time the writer lock is held.
const MAX_ADMISSION_BATCH: usize = 512;

/// Owns the leader role. If the drain panics, or the blocking task is dropped
/// before running (runtime shutdown), `Drop` releases the role and drops the
/// queued reply senders so every waiting producer gets an error, not a hang.
struct AdmissionLeader {
    queue: Arc<Mutex<AdmissionQueue>>,
    clean_exit: bool,
}

impl Drop for AdmissionLeader {
    fn drop(&mut self) {
        if !self.clean_exit {
            let mut guard = self.queue.lock();
            guard.pending.clear();
            guard.leader_active = false;
        }
    }
}

impl AdmissionLeader {
    fn drain(&mut self) {
        loop {
            let batch = {
                let mut guard = self.queue.lock();
                if guard.pending.is_empty() {
                    guard.leader_active = false;
                    self.clean_exit = true;
                    return;
                }
                // Group only entries for the first connection: tests may share
                // a coordinator across pools, production never does.
                let first = Arc::clone(&guard.pending[0].conn);
                let mut batch = Vec::new();
                let mut rest = Vec::new();
                for item in guard.pending.drain(..) {
                    if batch.len() < MAX_ADMISSION_BATCH && Arc::ptr_eq(&item.conn, &first) {
                        batch.push(item);
                    } else {
                        rest.push(item);
                    }
                }
                guard.pending = rest;
                batch
            };
            let payloads: Vec<&str> = batch.iter().map(|item| item.payload.as_str()).collect();
            #[cfg(feature = "benchmark-profile")]
            let queue_wait = batch[0].enqueued_at.elapsed();
            #[cfg(feature = "benchmark-profile")]
            let commit = {
                let wait_start = std::time::Instant::now();
                let mut conn_guard = batch[0].conn.lock();
                let lock_wait = wait_start.elapsed();
                let tx_start = std::time::Instant::now();
                let outcome = openproxy_db::usage_journal::append_batch(&mut conn_guard, &payloads);
                let tx_duration = tx_start.elapsed();
                (outcome, lock_wait, tx_duration)
            };
            #[cfg(not(feature = "benchmark-profile"))]
            let outcome =
                openproxy_db::usage_journal::append_batch(&mut batch[0].conn.lock(), &payloads);

            #[cfg(feature = "benchmark-profile")]
            let (outcome, lock_wait, tx_duration) = commit;

            #[cfg(feature = "benchmark-profile")]
            if outcome.is_ok() {
                crate::worker::profile::record_admission_commit(
                    batch.len(),
                    queue_wait,
                    lock_wait,
                    tx_duration,
                );
            }
            match outcome {
                Ok(results) => {
                    for (item, result) in batch.into_iter().zip(results) {
                        let _ = item.reply.send(result);
                    }
                }
                Err(error) => {
                    for item in batch {
                        let _ = item.reply.send(Err(error.clone()));
                    }
                }
            }
        }
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

        let admission_result = coord.admit(conn, payload.clone()).await;

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
