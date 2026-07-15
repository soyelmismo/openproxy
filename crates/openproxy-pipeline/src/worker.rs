use openproxy_types::config::CooldownMode;
use openproxy_types::SelectionRegistry;
use openproxy_types::usage::UsageInput;
use openproxy_types::ids::{ComboId, ComboTargetId};
use rusqlite::Connection;
use std::sync::Arc;
use tokio::sync::mpsc;

#[allow(clippy::large_enum_variant)]
pub enum BackgroundJob {
    RecordAttempt {
        usage_input: UsageInput,
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
    repo: Arc<dyn crate::repository::PipelineRepository>,
    mut rx: mpsc::Receiver<BackgroundJob>,
    selection_registry: Arc<SelectionRegistry>,
) {
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let conn_clone = conn.clone();
            let repo_clone = repo.clone();
            let selection_registry_clone = selection_registry.clone();

            // Usar spawn_blocking para las queries de SQLite
            let _ = tokio::task::spawn_blocking(move || {
                process_job(&conn_clone, repo_clone.as_ref(), job, selection_registry_clone);
            })
            .await;
        }
    });
}

pub fn process_job(
    conn_clone: &Arc<parking_lot::Mutex<Connection>>,
    repo: &dyn crate::repository::PipelineRepository,
    job: BackgroundJob,
    selection_registry: Arc<SelectionRegistry>,
) {
    match job {
        BackgroundJob::RecordAttempt {
            usage_input,
            target_id,
            combo_id: _,
            error_msg,
            is_upstream_health_issue,
            cooldown_mode,
            cooldown_base_secs,
            cooldown_max_secs,
            cooldown_factor,
        } => {
            let lock = conn_clone.lock();

            // 1. Record usage
            if let Err(e) = openproxy_db::cost::record(&lock, &usage_input) {
                tracing::warn!("failed to record usage in background: {}", e);
            }
            drop(lock);

            // 2. Update selection registry
            if error_msg.is_none() {
                selection_registry.record_success(target_id);
            } else {
                selection_registry.record_request(target_id);
            }

            // 3. Cooldown
            let cooldown_op = match error_msg {
                None => Some("clear"),
                Some(_) if is_upstream_health_issue => Some("record"),
                Some(_) => None,
            };

            if let Some(op) = cooldown_op {
                match op {
                    "clear" => {
                        if let Err(e) = repo.clear_cooldown(target_id) {
                            tracing::warn!("cooldown::clear failed in background: {}", e);
                        }
                    }
                    "record" => {
                        let reason = error_msg.unwrap_or_else(|| "retryable failure".to_string());
                        if let Err(e) = repo.record_cooldown(
                            target_id,
                            &reason,
                            cooldown_mode,
                            cooldown_base_secs,
                            cooldown_max_secs,
                            cooldown_factor,
                        ) {
                            tracing::warn!(
                                "cooldown::record failed in background: {}",
                                e
                            );
                        }
                    }
                    _ => {}
                }
            }
        }
        BackgroundJob::MarkClientResponse {
            request_id,
            attempt,
            target_id,
        } => {
            if let Err(e) = repo.mark_winner_usage_row(&request_id, attempt, target_id) {
                tracing::warn!("failed to mark client response in background: {}", e);
            }
        }
    }
}
