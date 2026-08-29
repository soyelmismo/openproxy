use openproxy_types::SelectionRegistry;
use openproxy_types::config::CooldownMode;
use openproxy_types::ids::{ComboId, ComboTargetId};
use openproxy_types::usage::UsageInput;
use rusqlite::Connection;
use std::sync::Arc;
use tokio::sync::mpsc;

pub enum BackgroundJob {
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
    repo: Arc<dyn crate::repository::PipelineRepository>,
    mut rx: mpsc::Receiver<BackgroundJob>,
    selection_registry: Arc<SelectionRegistry>,
) {
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let conn_clone = Arc::clone(&conn);
            let repo_clone = Arc::clone(&repo);
            let selection_registry_clone = Arc::clone(&selection_registry);

            // Usar spawn_blocking para las queries de SQLite
            let _ = tokio::task::spawn_blocking(move || {
                process_job(
                    &conn_clone,
                    repo_clone.as_ref(),
                    job,
                    &selection_registry_clone,
                );
            })
            .await;
        }
    });
}

struct CooldownParams {
    target_id: ComboTargetId,
    combo_id: ComboId,
    error_msg: Option<String>,
    is_upstream_health_issue: bool,
    cooldown_mode: CooldownMode,
    cooldown_base_secs: u64,
    cooldown_max_secs: u64,
    cooldown_factor: u32,
}

fn update_cooldown(repo: &dyn crate::repository::PipelineRepository, params: CooldownParams) {
    if params.combo_id.0 == -1 {
        return;
    }

    match params.error_msg {
        None => {
            if let Err(e) = repo.clear_cooldown(params.target_id) {
                tracing::warn!("cooldown::clear failed in background: {}", e);
            }
        }
        Some(reason) if params.is_upstream_health_issue => {
            if params.cooldown_mode != CooldownMode::None
                && params.cooldown_base_secs > 0
                && let Err(e) = repo.record_cooldown(
                    params.target_id,
                    &reason,
                    params.cooldown_mode,
                    params.cooldown_base_secs,
                    params.cooldown_max_secs,
                    params.cooldown_factor,
                )
            {
                tracing::warn!("cooldown::record failed in background: {}", e);
            }
        }
        Some(_) => {}
    }
}

fn handle_record_attempt(
    conn_clone: &Arc<parking_lot::Mutex<Connection>>,
    repo: &dyn crate::repository::PipelineRepository,
    selection_registry: &SelectionRegistry,
    usage_input: Box<UsageInput>,
    params: CooldownParams,
) {
    {
        let lock = parking_lot::Mutex::lock(conn_clone);
        if let Err(e) = openproxy_db::cost::record(&lock, &usage_input) {
            tracing::warn!("failed to record usage in background: {}", e);
        }
    }

    if params.error_msg.is_none() {
        selection_registry.record_success(params.target_id);
    } else {
        selection_registry.record_failure(params.target_id);
    }

    update_cooldown(repo, params);
}

pub fn process_job(
    conn_clone: &Arc<parking_lot::Mutex<Connection>>,
    repo: &dyn crate::repository::PipelineRepository,
    job: BackgroundJob,
    selection_registry: &SelectionRegistry,
) {
    match job {
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
        } => handle_record_attempt(
            conn_clone,
            repo,
            selection_registry,
            usage_input,
            CooldownParams {
                target_id,
                combo_id,
                error_msg,
                is_upstream_health_issue,
                cooldown_mode,
                cooldown_base_secs,
                cooldown_max_secs,
                cooldown_factor,
            },
        ),
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
