use openproxy_adapters::CancellationToken;
use openproxy_types::config::CooldownMode;
use openproxy_types::ids::{ComboId, ComboTargetId};
use openproxy_types::usage::UsageInput;
use rusqlite::Connection;
use std::sync::Arc;
use tokio::sync::mpsc;

pub mod coordinator;
pub use coordinator::*;

#[cfg(feature = "benchmark-profile")]
pub mod profile;

/// Closes admission, drains accepted jobs and waits for the SQLite worker.
pub struct WorkerHandle {
    cancel: CancellationToken,
    task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    failed_jobs: Arc<std::sync::atomic::AtomicU64>,
    conn: Arc<parking_lot::Mutex<Connection>>,
    coordinator: Arc<JournalCoordinator>,
}

impl WorkerHandle {
    pub fn coordinator(&self) -> &Arc<JournalCoordinator> {
        &self.coordinator
    }

    pub async fn stats(&self) -> openproxy_types::Result<WorkerStats> {
        let conn = Arc::clone(&self.conn);
        let pending =
            tokio::task::spawn_blocking(move || openproxy_db::usage_journal::depth(&conn.lock()))
                .await
                .map_err(|error| {
                    openproxy_types::CoreError::Internal(format!(
                        "usage stats join failed: {error}"
                    ))
                })??;
        Ok(WorkerStats {
            pending,
            failed_batches: self.failed_jobs.load(std::sync::atomic::Ordering::Relaxed),
        })
    }

    pub async fn shutdown(&self) -> openproxy_types::Result<()> {
        self.cancel.cancel();
        self.coordinator.notify_closed();
        let mut task = self.task.lock().await;
        if let Some(handle) = task.as_mut() {
            let result = handle.await;
            task.take();
            result.map_err(|error| {
                openproxy_types::CoreError::Internal(format!("usage worker join failed: {error}"))
            })?;
        }
        let stats = self.stats().await?;
        if stats.pending > 0 {
            return Err(openproxy_types::CoreError::Internal(format!(
                "usage worker stopped with {} durable pending jobs ({} failed batches)",
                stats.pending, stats.failed_batches
            )));
        }
        Ok(())
    }
}

#[derive(serde::Serialize)]
pub struct WorkerStats {
    pub pending: u64,
    pub failed_batches: u64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum BackgroundJob {
    JournalWake,
    RecordUsage(Box<UsageInput>),
    RecordAttempt {
        usage_input: Box<UsageInput>,
        target_id: ComboTargetId,
        combo_id: ComboId,
        error_msg: Option<String>,
        is_upstream_health_issue: bool,
        cooldown_mode: CooldownMode,
        cooldown_base_secs: u64,
        cooldown_max_secs: u64,
        cooldown_factor: u32,
    },
    MarkClientResponse {
        request_id: String,
        attempt: u8,
        target_id: ComboTargetId,
    },
}

pub fn spawn_worker(
    conn: Arc<parking_lot::Mutex<Connection>>,
    rx: mpsc::Receiver<BackgroundJob>,
) -> WorkerHandle {
    let coordinator = coordinator_for(&conn);
    spawn_worker_with_canonical_coordinator(conn, rx, coordinator)
}

pub fn spawn_worker_with_coordinator(
    conn: Arc<parking_lot::Mutex<Connection>>,
    rx: mpsc::Receiver<BackgroundJob>,
    coordinator: Arc<JournalCoordinator>,
) -> openproxy_types::Result<WorkerHandle> {
    let canonical = register_coordinator_for(&conn, coordinator)?;
    Ok(spawn_worker_with_canonical_coordinator(conn, rx, canonical))
}

fn spawn_worker_with_canonical_coordinator(
    conn: Arc<parking_lot::Mutex<Connection>>,
    mut rx: mpsc::Receiver<BackgroundJob>,
    coordinator: Arc<JournalCoordinator>,
) -> WorkerHandle {
    let cancel = CancellationToken::new();
    let shutdown = cancel.clone();
    let failed_jobs = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let failures = Arc::clone(&failed_jobs);
    let handle_conn = Arc::clone(&conn);
    let worker_coord = Arc::clone(&coordinator);
    let task = tokio::spawn(async move {
        let mut retry = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            let job = tokio::select! {
                biased;
                () = shutdown.cancelled(), if !rx.is_closed() => {
                    rx.close();
                    worker_coord.notify_closed();
                    continue;
                }
                job = rx.recv() => {
                    let Some(job) = job else {
                        let final_conn = Arc::clone(&conn);
                        let final_coord = Arc::clone(&worker_coord);
                        match tokio::task::spawn_blocking(move || replay_pending_with_coordinator(&final_conn, &final_coord)).await {
                            Ok(Ok(())) => {}
                            result => {
                                failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                tracing::error!(?result, "usage journal drain incomplete");
                            }
                        }
                        break
                    };
                    job
                }
                _ = retry.tick(), if !rx.is_closed() => BackgroundJob::JournalWake,
            };
            let mut batch = vec![job];
            while batch.len() < 1024 {
                match rx.try_recv() {
                    Ok(j) => batch.push(j),
                    Err(_) => break,
                }
            }

            let conn_clone = Arc::clone(&conn);
            let batch_failures = Arc::clone(&failures);
            let batch_coord = Arc::clone(&worker_coord);

            // spawn_blocking: SQLite es síncrono y no puede correr en el hilo de Tokio.
            // Wakes only signal that the journal has work: one replay drains
            // everything admitted so far, so the batch costs a single replay.
            let result = tokio::task::spawn_blocking(move || {
                for job in batch {
                    if matches!(job, BackgroundJob::JournalWake) {
                        continue;
                    }
                    if let Err(error) = admit_job(&conn_clone, &job) {
                        batch_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        tracing::error!(%error, "failed to admit usage job");
                    }
                }
                if let Err(error) = replay_pending_with_coordinator(&conn_clone, &batch_coord) {
                    batch_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tracing::error!(%error, "failed to persist usage job");
                }
            })
            .await;
            if let Err(error) = result {
                failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::error!(%error, "usage worker batch panicked");
                break;
            }
        }
    });
    WorkerHandle {
        cancel,
        task: tokio::sync::Mutex::new(Some(task)),
        failed_jobs,
        conn: handle_conn,
        coordinator,
    }
}

pub fn process_job(
    conn: &Arc<parking_lot::Mutex<Connection>>,
    job: BackgroundJob,
) -> openproxy_types::Result<()> {
    let coordinator = coordinator_for(conn);
    process_job_with_coordinator(conn, job, &coordinator)
}

pub fn process_job_with_coordinator(
    conn: &Arc<parking_lot::Mutex<Connection>>,
    job: BackgroundJob,
    coordinator: &JournalCoordinator,
) -> openproxy_types::Result<()> {
    if !matches!(job, BackgroundJob::JournalWake) {
        admit_job(conn, &job)?;
    }
    replay_pending_with_coordinator(conn, coordinator)
}

pub fn admit_job(
    conn: &Arc<parking_lot::Mutex<Connection>>,
    job: &BackgroundJob,
) -> openproxy_types::Result<i64> {
    let payload = serde_json::to_string(job).map_err(|error| {
        openproxy_types::CoreError::Internal(format!("usage serialization failed: {error}"))
    })?;
    openproxy_db::usage_journal::append(&mut conn.lock(), &payload)
}

pub fn replay_pending(conn: &Arc<parking_lot::Mutex<Connection>>) -> openproxy_types::Result<()> {
    let coordinator = coordinator_for(conn);
    replay_pending_with_coordinator(conn, &coordinator)
}

pub fn replay_pending_with_coordinator(
    conn: &Arc<parking_lot::Mutex<Connection>>,
    coordinator: &JournalCoordinator,
) -> openproxy_types::Result<()> {
    loop {
        let (applied, rows) = {
            #[cfg(feature = "benchmark-profile")]
            let lock_start = std::time::Instant::now();
            let mut connection = conn.lock();
            #[cfg(feature = "benchmark-profile")]
            let lock_wait = lock_start.elapsed();
            let entries = openproxy_db::usage_journal::pending(&connection, REPLAY_BATCH)?;
            if entries.is_empty() {
                return Ok(());
            }
            #[cfg(feature = "benchmark-profile")]
            let tx_start = std::time::Instant::now();
            match apply_batch(&mut connection, &entries) {
                Ok(rows) => {
                    #[cfg(feature = "benchmark-profile")]
                    crate::worker::profile::record_replay_commit(
                        entries.len(),
                        lock_wait,
                        tx_start.elapsed(),
                    );
                    (entries.len(), rows)
                }
                // One bad entry must not hide the good ones before it: replay
                // the page entry by entry, which commits each success and
                // stops at the failing entry exactly like the unbatched path.
                Err(_) if entries.len() > 1 => {
                    drop(connection);
                    return replay_one_by_one(conn, coordinator, entries);
                }
                Err(error) => return Err(error),
            }
        };
        for row in rows {
            openproxy_types::usage::publish_usage_row(row);
        }
        coordinator.notify_drained(applied);
    }
}

/// Journal entries applied per transaction. One commit (one fsync with
/// `synchronous=FULL`) covers the whole page instead of one per request.
const REPLAY_BATCH: usize = 256;

fn decode(id: i64, payload: &str) -> openproxy_types::Result<BackgroundJob> {
    serde_json::from_str(payload).map_err(|error| {
        openproxy_types::CoreError::Internal(format!("invalid usage journal entry {id}: {error}"))
    })
}

fn apply_batch(
    connection: &mut Connection,
    entries: &[(i64, String)],
) -> openproxy_types::Result<Vec<openproxy_types::usage::RecentUsageRow>> {
    openproxy_db::with_busy_retry("usage_worker::apply_batch", || {
        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(openproxy_db::error::map_db_error)?;
        let mut rows = Vec::new();
        for (id, payload) in entries {
            if let Some(row) = apply_entry(&transaction, decode(*id, payload)?, *id)? {
                rows.push(row);
            }
        }
        transaction
            .commit()
            .map_err(openproxy_db::error::map_db_error)?;
        Ok(rows)
    })
}

fn replay_one_by_one(
    conn: &Arc<parking_lot::Mutex<Connection>>,
    coordinator: &JournalCoordinator,
    entries: Vec<(i64, String)>,
) -> openproxy_types::Result<()> {
    for (id, payload) in entries {
        let rows = apply_batch(&mut conn.lock(), &[(id, payload)])?;
        for row in rows {
            openproxy_types::usage::publish_usage_row(row);
        }
        coordinator.notify_drained(1);
    }
    replay_pending_with_coordinator(conn, coordinator)
}

/// Applies one journal entry inside the caller's transaction and acknowledges
/// it there. Already-acknowledged entries are skipped (idempotent replay).
fn apply_entry(
    transaction: &Connection,
    job: BackgroundJob,
    journal_id: i64,
) -> openproxy_types::Result<Option<openproxy_types::usage::RecentUsageRow>> {
    use openproxy_db::usage_writer::{AttemptCooldown, record_in_transaction};
    match job {
        BackgroundJob::JournalWake => Err(openproxy_types::CoreError::Internal(
            "wake cannot be journaled".into(),
        )),
        BackgroundJob::RecordUsage(input) => {
            Ok(record_in_transaction(transaction, &input, None, Some(journal_id))?.map(|r| r.1))
        }
        BackgroundJob::RecordAttempt {
            usage_input,
            target_id,
            combo_id,
            error_msg,
            is_upstream_health_issue,
            cooldown_mode,
            cooldown_base_secs,
            cooldown_max_secs,
            cooldown_factor,
        } => {
            let cooldown = AttemptCooldown {
                target_id,
                combo_id,
                error: error_msg.as_deref(),
                is_upstream_health_issue,
                mode: cooldown_mode,
                base_secs: cooldown_base_secs,
                max_secs: cooldown_max_secs,
                factor: cooldown_factor,
            };
            Ok(
                record_in_transaction(
                    transaction,
                    &usage_input,
                    Some(&cooldown),
                    Some(journal_id),
                )?
                .map(|r| r.1),
            )
        }
        BackgroundJob::MarkClientResponse {
            request_id,
            attempt,
            target_id,
        } => {
            if openproxy_db::usage_journal::contains(transaction, journal_id)? {
                openproxy_db::cost::mark_winner_usage_row(
                    transaction,
                    &request_id,
                    attempt,
                    target_id,
                )?;
                openproxy_db::usage_journal::acknowledge(transaction, journal_id)?;
            }
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests;
