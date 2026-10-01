use openproxy_adapters::CancellationToken;
use openproxy_types::config::CooldownMode;
use openproxy_types::ids::{ComboId, ComboTargetId};
use openproxy_types::usage::UsageInput;
use rusqlite::Connection;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Closes admission, drains accepted jobs and waits for the SQLite worker.
pub struct WorkerHandle {
    cancel: CancellationToken,
    task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    failed_jobs: Arc<std::sync::atomic::AtomicU64>,
}

impl WorkerHandle {
    pub async fn shutdown(&self) -> openproxy_types::Result<()> {
        self.cancel.cancel();
        let mut task = self.task.lock().await;
        if let Some(handle) = task.as_mut() {
            let result = handle.await;
            task.take();
            result.map_err(|error| {
                openproxy_types::CoreError::Internal(format!("usage worker join failed: {error}"))
            })?;
        }
        let failures = self.failed_jobs.load(std::sync::atomic::Ordering::Relaxed);
        if failures > 0 {
            return Err(openproxy_types::CoreError::Internal(format!(
                "usage worker failed to persist {failures} jobs"
            )));
        }
        Ok(())
    }
}

pub enum BackgroundJob {
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
    mut rx: mpsc::Receiver<BackgroundJob>,
) -> WorkerHandle {
    let cancel = CancellationToken::new();
    let shutdown = cancel.clone();
    let failed_jobs = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let failures = Arc::clone(&failed_jobs);
    let task = tokio::spawn(async move {
        loop {
            let job = tokio::select! {
                biased;
                () = shutdown.cancelled(), if !rx.is_closed() => {
                    rx.close();
                    continue;
                }
                job = rx.recv() => {
                    let Some(job) = job else { break };
                    job
                }
            };
            let mut batch = vec![job];
            while batch.len() < 32 {
                match rx.try_recv() {
                    Ok(j) => batch.push(j),
                    Err(_) => break,
                }
            }

            let conn_clone = Arc::clone(&conn);
            let batch_failures = Arc::clone(&failures);

            // spawn_blocking: SQLite es síncrono y no puede correr en el hilo de Tokio.
            let result = tokio::task::spawn_blocking(move || {
                for job in batch {
                    if let Err(error) = process_job(&conn_clone, job) {
                        batch_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        tracing::error!(%error, "failed to persist usage job");
                    }
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
    }
}

fn record_usage(
    conn: &Arc<parking_lot::Mutex<Connection>>,
    input: &UsageInput,
    cooldown: Option<&openproxy_db::usage_writer::AttemptCooldown<'_>>,
) -> openproxy_types::Result<()> {
    let (_, row) = {
        let mut connection = conn.lock();
        openproxy_db::usage_writer::record(&mut connection, input, cooldown)?
    };
    openproxy_types::usage::publish_usage_row(row);
    Ok(())
}

pub fn process_job(
    conn: &Arc<parking_lot::Mutex<Connection>>,
    job: BackgroundJob,
) -> openproxy_types::Result<()> {
    match job {
        BackgroundJob::RecordUsage(input) => record_usage(conn, &input, None),
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
        } => record_usage(
            conn,
            &usage_input,
            Some(&openproxy_db::usage_writer::AttemptCooldown {
                target_id,
                combo_id,
                error: error_msg.as_deref(),
                is_upstream_health_issue,
                mode: cooldown_mode,
                base_secs: cooldown_base_secs,
                max_secs: cooldown_max_secs,
                factor: cooldown_factor,
            }),
        ),
        BackgroundJob::MarkClientResponse {
            request_id,
            attempt,
            target_id,
        } => {
            let connection = conn.lock();
            openproxy_db::with_busy_retry("usage_worker::mark_winner", || {
                openproxy_db::cost::mark_winner_usage_row(
                    &connection,
                    &request_id,
                    attempt,
                    target_id,
                )
            })
        }
    }
}
