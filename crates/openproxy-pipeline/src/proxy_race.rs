use crate::{PipelineRequest, PipelineResult};
use openproxy_types::combos::Combo;
use openproxy_types::error::CoreError;
use openproxy_types::ids::TraceId;
use std::sync::Arc;

struct ProxyRaceContext {
    pipeline: crate::Pipeline,
    combo: Combo,
    target: crate::context::ResolvedTarget,
    proxy_id: String,
    winner: Arc<parking_lot::Mutex<Option<(PipelineResult, String)>>>,
    last_err: Arc<parking_lot::Mutex<Option<CoreError>>>,
    running: Arc<std::sync::atomic::AtomicUsize>,
    all_done: Arc<tokio::sync::Notify>,
    token: openproxy_adapters::upstream::CancellationToken,
    overall_attempt: u8,
    num_workers: u8,
}

async fn execute_proxy_worker(req: PipelineRequest, ctx: ProxyRaceContext) {
    if ctx.token.is_cancelled() {
        crate::racing::notify_worker_done!(ctx.running, ctx.all_done);
        return;
    }

    let result = ctx
        .pipeline
        .execute_single(crate::SingleExecutionParams {
            req,
            combo: &ctx.combo,
            resolved_target: &ctx.target,
            attempt: ctx.overall_attempt,
            race_size: ctx.num_workers,
            total_targets: 1,
            race_cancel: &ctx.token,
        })
        .await;

    if result.error.is_none() {
        let mut w = ctx.winner.lock();
        if w.is_none() {
            *w = Some((result, ctx.proxy_id));
        }
        ctx.running
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        ctx.all_done.notify_one();
        return;
    }

    if let Some(e) = &result.error {
        *ctx.last_err.lock() = Some(e.clone_for_result());
    }

    crate::racing::notify_worker_done!(ctx.running, ctx.all_done);
}

fn handle_proxy_race_winner(
    pipeline: &crate::Pipeline,
    target: &crate::context::ResolvedTarget,
    result: PipelineResult,
    win_proxy_id: String,
) -> PipelineResult {
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
    result
}

pub async fn run_proxy_race(
    pipeline: &crate::Pipeline,
    req: PipelineRequest,
    combo: &Combo,
    target: &crate::context::ResolvedTarget,
    candidate_proxies: Vec<(String, String)>,
    overall_attempt: u8,
) -> PipelineResult {
    use std::sync::atomic::AtomicUsize;
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
        let mut worker_req = req.clone();
        let handle = race_sink.handle(worker_idx);
        worker_req.proxy_override = Some((proxy_id.clone(), proxy_url));
        worker_req.stream_sink = Some(crate::race_sink::StreamSink::Race(handle));
        worker_req.race_cancel = Some(openproxy_adapters::upstream::CancellationToken::clone(
            &worker_tokens[worker_idx],
        ));
        worker_req.trace_id = TraceId::new();
        worker_req.race_cancelled = true;

        let ctx = ProxyRaceContext {
            pipeline: pipeline.clone(),
            combo: combo.clone(),
            target: target.clone(),
            proxy_id,
            winner: Arc::clone(&winner),
            last_err: Arc::clone(&last_err),
            running: Arc::clone(&running),
            all_done: Arc::clone(&all_done),
            token: worker_tokens[worker_idx].clone(),
            overall_attempt,
            num_workers: num_workers as u8,
        };

        set.spawn(execute_proxy_worker(worker_req, ctx));
    }

    let default_err = CoreError::NoHealthyTargets(combo.id.0);

    match crate::racing::wait_for_race_winner(
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
        Ok((result, win_proxy_id)) => {
            handle_proxy_race_winner(pipeline, target, result, win_proxy_id)
        }
        Err(err) => PipelineResult {
            status_code: err.http_status(),
            error: Some(err),
            final_response: None,
            attempts: overall_attempt,
            usage_tuple: None,
        },
    }
}
