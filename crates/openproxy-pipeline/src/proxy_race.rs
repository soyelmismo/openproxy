use crate::{PipelineRequest, PipelineResult};
use openproxy_types::combos::Combo;
use openproxy_types::error::CoreError;
use openproxy_types::ids::TraceId;
use std::sync::Arc;

pub async fn run_proxy_race(
    pipeline: &crate::Pipeline,
    req: PipelineRequest,
    combo: &Combo,
    target: &crate::context::ResolvedTarget,
    candidate_proxies: Vec<(String, String)>,
    overall_attempt: u8,
) -> PipelineResult {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    let num_workers = candidate_proxies.len();
    if num_workers == 0 {
        return PipelineResult {
            status_code: 502,
            error: Some(CoreError::NoHealthyTargets(combo.id.0)),
            final_response: None,
            attempts: overall_attempt,
            usage_tuple: None,
        };
    }

    let last_err: Arc<parking_lot::Mutex<Option<CoreError>>> =
        Arc::new(parking_lot::Mutex::new(None));
    let running = Arc::new(AtomicUsize::new(num_workers));
    let all_done = Arc::new(Notify::new());
    let winner: Arc<parking_lot::Mutex<Option<(PipelineResult, String)>>> =
        Arc::new(parking_lot::Mutex::new(None));

    let mut set: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    let original_tx = match req.stream_sink.as_ref() {
        Some(crate::race_sink::StreamSink::Direct(tx)) => tx.clone(),
        _ => {
            tracing::warn!("run_proxy_race: non-direct stream sink, using dummy channel");
            let (tx, _rx) = tokio::sync::mpsc::channel(32);
            tx
        }
    };

    let (race_sink, worker_tokens) = crate::race_sink::RaceSink::new(original_tx, num_workers);

    for (worker_idx, (proxy_id, proxy_url)) in candidate_proxies.into_iter().enumerate() {
        let mut req = req.clone();
        let handle = race_sink.handle(worker_idx);
        req.proxy_override = Some((proxy_id.clone(), proxy_url));
        req.stream_sink = Some(crate::race_sink::StreamSink::Race(handle));
        req.race_cancel = Some(openproxy_adapters::upstream::CancellationToken::clone(
            &worker_tokens[worker_idx],
        ));
        req.trace_id = TraceId::new();
        req.race_cancelled = true;

        let combo = combo.clone();
        let target = target.clone();
        let p = pipeline.clone();
        let winner = Arc::clone(&winner);
        let last_err = Arc::clone(&last_err);
        let running = Arc::clone(&running);
        let all_done = Arc::clone(&all_done);
        let token = worker_tokens[worker_idx].clone();

        set.spawn(async move {
            if token.is_cancelled() {
                if running.fetch_sub(1, Ordering::AcqRel) == 1 {
                    all_done.notify_one();
                }
                return;
            }

            let result = p
                .execute_single(crate::SingleExecutionParams {
                    req,
                    combo: &combo,
                    resolved_target: &target,
                    attempt: overall_attempt,
                    race_size: num_workers as u8,
                    total_targets: 1,
                    race_cancel: &token,
                })
                .await;

            if result.error.is_none() {
                let mut w = winner.lock();
                if w.is_none() {
                    *w = Some((result, proxy_id));
                }
                running.fetch_sub(1, Ordering::AcqRel);
                all_done.notify_one();
                return;
            }

            if let Some(e) = &result.error {
                *last_err.lock() = Some(e.clone_for_result());
            }

            if running.fetch_sub(1, Ordering::AcqRel) == 1 {
                all_done.notify_one();
            }
        });
    }

    loop {
        {
            let mut w = winner.lock();
            if let Some((result, win_proxy_id)) = w.take() {
                for token in &worker_tokens {
                    token.cancel();
                }
                {
                    let conn = pipeline.conn.lock();
                    let _ = openproxy_db::providers::update_current_proxy(
                        &conn,
                        &target.target.provider_id,
                        Some(&win_proxy_id),
                    );
                    tracing::info!(
                        provider = %target.target.provider_id,
                        proxy_id = %win_proxy_id,
                        "incremental proxy race won: updated provider current_proxy_id"
                    );
                }
                let grace =
                    std::time::Duration::from_millis(pipeline.config.racing.abort_grace_ms.max(50));
                let mut set = set;
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(grace, async {
                        while set.join_next().await.is_some() {}
                    })
                    .await;
                    set.abort_all();
                });
                return result;
            }
        }
        if running.load(Ordering::Acquire) == 0 {
            for token in &worker_tokens {
                token.cancel();
            }
            let err = last_err
                .lock()
                .take()
                .unwrap_or(CoreError::NoHealthyTargets(combo.id.0));
            return PipelineResult {
                status_code: err.http_status(),
                error: Some(err),
                final_response: None,
                attempts: overall_attempt,
                usage_tuple: None,
            };
        }
        all_done.notified().await;
    }
}
