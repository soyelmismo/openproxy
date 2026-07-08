use crate::combos::Combo;
use crate::error::CoreError;
use crate::ids::TraceId;
use crate::pipeline::{PipelineRequest, PipelineResult};
use std::sync::Arc;

pub(crate) async fn run_race(
    pipeline: &crate::pipeline::Pipeline,
    req: PipelineRequest,
    combo: &Combo,
    to_run: Vec<crate::pipeline::context::ResolvedTarget>,
    race_size: u8,
) -> PipelineResult {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    let num_workers = race_size.min(to_run.len() as u8);
    if num_workers == 0 {
        return PipelineResult {
            status_code: 502,
            error: Some(CoreError::NoHealthyTargets(combo.id.0)),
            final_response: None,
            attempts: 0,
            usage_tuple: None,
        };
    }

    let queue: Arc<parking_lot::Mutex<VecDeque<crate::pipeline::context::ResolvedTarget>>> =
        Arc::new(parking_lot::Mutex::new(VecDeque::from(to_run)));
    let last_err: Arc<parking_lot::Mutex<Option<CoreError>>> =
        Arc::new(parking_lot::Mutex::new(None));
    let running = Arc::new(AtomicUsize::new(num_workers as usize));
    let all_done = Arc::new(Notify::new());
    let winner: Arc<parking_lot::Mutex<Option<PipelineResult>>> =
        Arc::new(parking_lot::Mutex::new(None));

    let mut set: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    let original_tx = match req.stream_sink.as_ref() {
        Some(crate::race_sink::StreamSink::Direct(tx)) => tx.clone(),
        _ => {
            tracing::error!("run_race: expected StreamSink::Direct for original sink");
            return PipelineResult {
                status_code: 502,
                error: Some(CoreError::Internal(
                    "run_race: missing direct stream sink".into(),
                )),
                final_response: None,
                attempts: 0,
                usage_tuple: None,
            };
        }
    };

    let (race_sink, worker_tokens) =
        crate::race_sink::RaceSink::new(original_tx, num_workers as usize);

    #[allow(clippy::needless_range_loop)]
    for worker_idx in 0..num_workers as usize {
        let p = pipeline.clone();
        let mut req = req.clone();

        let handle = race_sink.handle(worker_idx);
        req.stream_sink = Some(crate::race_sink::StreamSink::Race(handle));
        req.race_cancel = Some(worker_tokens[worker_idx].clone());

        let combo = combo.clone();
        let queue = queue.clone();
        let winner = winner.clone();
        let last_err = last_err.clone();
        let running = running.clone();
        let all_done = all_done.clone();

        set.spawn(async move {
            loop {
                let worker_token = req
                    .race_cancel
                    .as_ref()
                    .expect("run_race: worker must have race_cancel")
                    .clone();
                if worker_token.is_cancelled() {
                    if running.fetch_sub(1, Ordering::AcqRel) == 1 {
                        all_done.notify_one();
                    }
                    return;
                }

                let target = queue.lock().pop_front();
                let Some(target) = target else {
                    if running.fetch_sub(1, Ordering::AcqRel) == 1 {
                        all_done.notify_one();
                    }
                    return;
                };

                req.trace_id = TraceId::new();
                req.race_cancelled = true;

                if worker_token.is_cancelled() {
                    if running.fetch_sub(1, Ordering::AcqRel) == 1 {
                        all_done.notify_one();
                    }
                    return;
                }

                let _req_arc = Arc::new(req.clone());
                let result = p
                    .execute_single(req.clone(), &combo, &target, 1, race_size, &worker_token)
                    .await;

                if result.error.is_none() {
                    if winner.lock().is_none() {
                        *winner.lock() = Some(result);
                    }
                    if running.fetch_sub(1, Ordering::AcqRel) == 1 {
                        all_done.notify_one();
                    }
                    return;
                }

                if let Some(e) = &result.error {
                    *last_err.lock() = Some(e.clone_for_result());
                }
            }
        });
    }

    loop {
        {
            let mut w = winner.lock();
            if let Some(result) = w.take() {
                for token in &worker_tokens {
                    token.cancel();
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
                attempts: race_size,
                usage_tuple: None,
            };
        }
        all_done.notified().await;
    }
}
