use crate::{PipelineRequest, PipelineResult};
use openproxy_types::combos::Combo;
use openproxy_types::error::CoreError;
use openproxy_types::ids::TraceId;
use std::sync::Arc;

macro_rules! notify_worker_done {
    ($running:expr, $all_done:expr) => {
        if $running.fetch_sub(1, std::sync::atomic::Ordering::AcqRel) == 1 {
            $all_done.notify_one();
        }
    };
}
pub(crate) use notify_worker_done;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn wait_for_race_winner<T>(
    winner: Arc<parking_lot::Mutex<Option<T>>>,
    running: Arc<std::sync::atomic::AtomicUsize>,
    all_done: Arc<tokio::sync::Notify>,
    worker_tokens: Vec<openproxy_adapters::upstream::CancellationToken>,
    last_err: Arc<parking_lot::Mutex<Option<CoreError>>>,
    set: tokio::task::JoinSet<()>,
    abort_grace_ms: u64,
    default_err: CoreError,
) -> Result<T, CoreError> {
    loop {
        if let Some(result) = winner.lock().take() {
            for token in &worker_tokens {
                token.cancel();
            }
            spawn_graceful_drain(set, abort_grace_ms);
            return Ok(result);
        }
        if running.load(std::sync::atomic::Ordering::Acquire) == 0 {
            for token in &worker_tokens {
                token.cancel();
            }
            let err = last_err.lock().take().unwrap_or(default_err);
            return Err(err);
        }
        all_done.notified().await;
    }
}

pub(crate) fn spawn_graceful_drain(mut set: tokio::task::JoinSet<()>, abort_grace_ms: u64) {
    let grace = std::time::Duration::from_millis(abort_grace_ms.max(50));
    tokio::spawn(async move {
        let _ =
            tokio::time::timeout(grace, async { while set.join_next().await.is_some() {} }).await;
        set.abort_all();
    });
}

struct RaceContext {
    pipeline: crate::Pipeline,
    combo: Combo,
    queue: Arc<parking_lot::Mutex<std::collections::VecDeque<crate::context::ResolvedTarget>>>,
    winner: Arc<parking_lot::Mutex<Option<PipelineResult>>>,
    last_err: Arc<parking_lot::Mutex<Option<CoreError>>>,
    running: Arc<std::sync::atomic::AtomicUsize>,
    all_done: Arc<tokio::sync::Notify>,
    race_size: u8,
    total_targets: u8,
}

async fn execute_race_worker(mut req: PipelineRequest, ctx: RaceContext) {
    let worker_token = req
        .race_cancel
        .clone()
        .expect("run_race: worker must have race_cancel");
    loop {
        if worker_token.is_cancelled() {
            notify_worker_done!(ctx.running, ctx.all_done);
            return;
        }

        let target = ctx.queue.lock().pop_front();
        let Some(target) = target else {
            notify_worker_done!(ctx.running, ctx.all_done);
            return;
        };

        req.trace_id = TraceId::new();
        req.race_cancelled = true;

        if worker_token.is_cancelled() {
            notify_worker_done!(ctx.running, ctx.all_done);
            return;
        }

        let result = ctx
            .pipeline
            .execute_single(crate::SingleExecutionParams {
                req: req.clone(),
                combo: &ctx.combo,
                resolved_target: &target,
                attempt: 1,
                race_size: ctx.race_size,
                total_targets: ctx.total_targets,
                race_cancel: &worker_token,
            })
            .await;

        if result.error.is_none() {
            let mut w = ctx.winner.lock();
            if w.is_none() {
                *w = Some(result);
            }
            notify_worker_done!(ctx.running, ctx.all_done);
            return;
        }

        if let Some(e) = &result.error {
            *ctx.last_err.lock() = Some(e.clone_for_result());
        }
    }
}

fn extract_direct_sink(
    req: &PipelineRequest,
) -> Option<tokio::sync::mpsc::Sender<bytes::Bytes>> {
    match req.stream_sink.as_ref() {
        Some(crate::race_sink::StreamSink::Direct(tx)) => Some(tx.clone()),
        _ => {
            tracing::error!("run_race: expected StreamSink::Direct for original sink");
            None
        }
    }
}

pub(crate) async fn run_race(
    pipeline: &crate::Pipeline,
    req: PipelineRequest,
    combo: &Combo,
    to_run: Vec<crate::context::ResolvedTarget>,
    race_size: u8,
) -> PipelineResult {
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;
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

    let Some(original_tx) = extract_direct_sink(&req) else {
        return PipelineResult {
            status_code: 502,
            error: Some(CoreError::Internal(
                "run_race: missing direct stream sink".into(),
            )),
            final_response: None,
            attempts: 0,
            usage_tuple: None,
        };
    };

    let total_targets = to_run.len() as u8;
    let queue = Arc::new(parking_lot::Mutex::new(VecDeque::from(to_run)));
    let last_err = Arc::new(parking_lot::Mutex::new(None));
    let running = Arc::new(AtomicUsize::new(num_workers as usize));
    let all_done = Arc::new(Notify::new());
    let winner = Arc::new(parking_lot::Mutex::new(None));

    let mut set: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    let (race_sink, worker_tokens) =
        crate::race_sink::RaceSink::new(original_tx, num_workers as usize);

    for (worker_idx, token) in worker_tokens.iter().enumerate() {
        let mut worker_req = req.clone();
        let handle = race_sink.handle(worker_idx);
        worker_req.stream_sink = Some(crate::race_sink::StreamSink::Race(handle));
        worker_req.race_cancel = Some(openproxy_adapters::upstream::CancellationToken::clone(
            token,
        ));

        let ctx = RaceContext {
            pipeline: pipeline.clone(),
            combo: combo.clone(),
            queue: Arc::clone(&queue),
            winner: Arc::clone(&winner),
            last_err: Arc::clone(&last_err),
            running: Arc::clone(&running),
            all_done: Arc::clone(&all_done),
            race_size,
            total_targets,
        };

        set.spawn(execute_race_worker(worker_req, ctx));
    }

    let default_err = CoreError::NoHealthyTargets(combo.id.0);

    match wait_for_race_winner(
        winner,
        running,
        all_done,
        worker_tokens,
        last_err,
        set,
        pipeline.config.racing.abort_grace_ms,
        default_err,
    )
    .await
    {
        Ok(result) => result,
        Err(err) => PipelineResult {
            status_code: err.http_status(),
            error: Some(err),
            final_response: None,
            attempts: race_size,
            usage_tuple: None,
        },
    }
}
