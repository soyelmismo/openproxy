use crate::ErrorPhase;
use crate::circuit_breaker::Health;
use crate::{Pipeline, PipelineRequest, PipelineResult};
use openproxy_types::combos::{Combo, ComboTarget, Strategy};
use openproxy_types::error::CoreError;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Request state passed down the middleware chain.
pub struct PipelineState {
    pub req: PipelineRequest,
    pub combo: Option<Combo>,
    pub eligible_targets: Option<Vec<crate::context::ResolvedTarget>>,
    pub race_size: Option<usize>,
}

// =====================================================================
// Error Telemetry Service
// =====================================================================

#[derive(Clone)]
pub struct ErrorTelemetryService<S> {
    pub pipeline: Pipeline,
    pub inner: S,
}

impl<S> ErrorTelemetryService<S> {
    pub fn new(pipeline: Pipeline, inner: S) -> Self {
        Self { pipeline, inner }
    }
}

impl<S> tower::Service<PipelineState> for ErrorTelemetryService<S>
where
    S: tower::Service<PipelineState, Response = PipelineResult, Error = std::convert::Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = PipelineResult;
    type Error = std::convert::Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, state: PipelineState) -> Self::Future {
        let pipeline = self.pipeline.clone();
        let mut inner = self.inner.clone();

        let request_id = state.req.request_id.to_string();
        let trace_id = state.req.trace_id.to_string();
        // Since state is moved, we keep a cloned combo if available.
        let combo = state.combo.clone();

        let started = std::time::Instant::now();

        Box::pin(async move {
            let res = inner.call(state).await?;

            if let Some(err) = &res.error
                && matches!(err, CoreError::NoHealthyTargets(_))
            {
                if let Some(c) = combo {
                    let repo = pipeline.repo().clone();
                    let req_id = request_id.clone();
                    let tr_id = trace_id.clone();
                    let c = c.clone();
                    let elapsed = started.elapsed().as_millis() as u64;
                    let created = chrono::Utc::now().naive_utc().to_string();
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = repo.record_no_healthy_targets_row(
                            &req_id,
                            &tr_id,
                            &c,
                            elapsed,
                            &created,
                            "No healthy targets available",
                        );
                    })
                    .await;
                } else {
                    // If combo wasn't populated yet, we can't record it identically,
                    // but NoHealthyTargets only occurs after combo is resolved.
                }
            }
            Ok(res)
        })
    }
}

pub struct ErrorTelemetryLayer {
    pub pipeline: Pipeline,
}

impl ErrorTelemetryLayer {
    pub fn new(pipeline: Pipeline) -> Self {
        Self { pipeline }
    }
}

impl<S> tower::Layer<S> for ErrorTelemetryLayer {
    type Service = ErrorTelemetryService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ErrorTelemetryService::new(self.pipeline.clone(), inner)
    }
}

// =====================================================================
// Resolve Service
// =====================================================================

#[derive(Clone)]
pub struct ResolveService<S> {
    pub pipeline: Pipeline,
    pub inner: S,
}

impl<S> ResolveService<S> {
    pub fn new(pipeline: Pipeline, inner: S) -> Self {
        Self { pipeline, inner }
    }
}

impl<S> tower::Service<PipelineState> for ResolveService<S>
where
    S: tower::Service<PipelineState, Response = PipelineResult, Error = std::convert::Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = PipelineResult;
    type Error = std::convert::Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut state: PipelineState) -> Self::Future {
        let pipeline = self.pipeline.clone();
        let mut inner = self.inner.clone();

        Box::pin(async move {
            // 1. Resolve the combo.
            let combo = match pipeline.load_combo(&state.req).await {
                Ok(c) => c,
                Err(e) => return Ok(pipeline.failure(e, 0, ErrorPhase::Resolve)),
            };

            let attempt: u8 = 1;

            // 2. Resolve and expand targets.
            let targets = match pipeline
                .resolve_targets(&combo, state.req.targets_override.as_deref())
                .await
            {
                Ok(t) => t,
                Err(e) => return Ok(pipeline.failure(e, attempt - 1, ErrorPhase::Resolve)),
            };

            // 3. Flatten sub-combos.
            let flat_targets = match pipeline.flatten_targets(&combo.id, targets.clone()).await {
                Ok(t) => t,
                Err(e) => return Ok(pipeline.failure(e, attempt - 1, ErrorPhase::Resolve)),
            };

            // 4. Filter out accounts that the circuit breaker marks unhealthy.
            let pre_cb_snapshot: Vec<ComboTarget> = flat_targets.clone();
            let mut eligible: Vec<ComboTarget> = flat_targets
                .into_iter()
                .filter(|t| match t.account_id {
                    Some(aid) => {
                        let key = if t.rate_limit_scope
                            == openproxy_types::providers::RateLimitScope::Model
                        {
                            crate::circuit_breaker::CircuitBreakerKey::Model(
                                aid,
                                t.model_row_id.expect("flattened"),
                            )
                        } else {
                            crate::circuit_breaker::CircuitBreakerKey::Account(aid)
                        };
                        pipeline.circuit_breaker.is_healthy(key) == Health::Healthy
                    }
                    None => true,
                })
                .collect();

            if eligible.is_empty() && !pre_cb_snapshot.is_empty() {
                tracing::warn!(
                    combo_id = combo.id.0,
                    parked = pre_cb_snapshot.len(),
                    "all targets' accounts unhealthy in circuit_breaker; falling through to pre-CB dispatch"
                );
                eligible = pre_cb_snapshot.clone();
            }

            if eligible.is_empty() {
                if attempt == 1 {
                    let repopulated = match tokio::task::spawn_blocking({
                        let p = pipeline.clone();
                        let cid = combo.id;
                        move || p.repo().auto_populate_empty_combo(cid)
                    })
                    .await
                    .unwrap()
                    {
                        Ok(n) => n,
                        Err(e) => {
                            tracing::warn!(
                                combo_id = combo.id.0,
                                combo_name = %combo.name,
                                error = %e,
                                "auto_populate on NoHealthyTargets failed; recording failure"
                            );
                            return Ok(pipeline.failure(e, attempt - 1, ErrorPhase::Route));
                        }
                    };
                    if repopulated > 0 {
                        let targets = match pipeline
                            .resolve_targets(&combo, state.req.targets_override.as_deref())
                            .await
                        {
                            Ok(t) => t,
                            Err(e) => {
                                return Ok(pipeline.failure(e, attempt - 1, ErrorPhase::Resolve));
                            }
                        };
                        let flat_targets = match pipeline.flatten_targets(&combo.id, targets).await
                        {
                            Ok(t) => t,
                            Err(e) => {
                                return Ok(pipeline.failure(e, attempt - 1, ErrorPhase::Resolve));
                            }
                        };
                        let re_eligible: Vec<ComboTarget> = flat_targets
                            .into_iter()
                            .filter(|t| match t.account_id {
                                Some(aid) => {
                                    let key = if t.rate_limit_scope
                                        == openproxy_types::providers::RateLimitScope::Model
                                    {
                                        crate::circuit_breaker::CircuitBreakerKey::Model(
                                            aid,
                                            t.model_row_id.expect("flattened"),
                                        )
                                    } else {
                                        crate::circuit_breaker::CircuitBreakerKey::Account(aid)
                                    };
                                    pipeline.circuit_breaker.is_healthy(key) == Health::Healthy
                                }
                                None => true,
                            })
                            .collect();
                        if !re_eligible.is_empty() {
                            eligible = re_eligible;
                        }
                    }
                }
                if eligible.is_empty() {
                    let err = CoreError::NoHealthyTargets(combo.id.0);
                    return Ok(pipeline.failure(err, attempt - 1, ErrorPhase::Route));
                }
            }

            let resolved = pipeline.resolve_combo_targets_full(eligible).await;

            if resolved.is_empty() {
                let err = CoreError::NoHealthyTargets(combo.id.0);
                return Ok(pipeline.failure(err, attempt - 1, ErrorPhase::Route));
            }

            state.combo = Some(combo);
            state.eligible_targets = Some(resolved);

            inner.call(state).await
        })
    }
}

pub struct ResolveLayer {
    pub pipeline: Pipeline,
}

impl ResolveLayer {
    pub fn new(pipeline: Pipeline) -> Self {
        Self { pipeline }
    }
}

impl<S> tower::Layer<S> for ResolveLayer {
    type Service = ResolveService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ResolveService::new(self.pipeline.clone(), inner)
    }
}

// =====================================================================
// Quota Service
// =====================================================================

#[derive(Clone)]
pub struct QuotaService<S> {
    pub pipeline: Pipeline,
    pub inner: S,
}

impl<S> QuotaService<S> {
    pub fn new(pipeline: Pipeline, inner: S) -> Self {
        Self { pipeline, inner }
    }
}

impl<S> tower::Service<PipelineState> for QuotaService<S>
where
    S: tower::Service<PipelineState, Response = PipelineResult, Error = std::convert::Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = PipelineResult;
    type Error = std::convert::Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut state: PipelineState) -> Self::Future {
        let pipeline = self.pipeline.clone();
        let mut inner = self.inner.clone();

        Box::pin(async move {
            let combo = state.combo.as_ref().unwrap();
            let eligible = state.eligible_targets.take().unwrap();
            let attempt: u8 = 1;

            let filtered = {
                let master_key = pipeline.config.master_key.clone();
                let repo = pipeline.repo().clone();
                let enabled = pipeline.config.quota_protection.enabled;
                let threshold = pipeline.config.quota_protection.threshold_percentage;
                let req_model = state.req.openai_request.model.clone();
                tokio::task::spawn_blocking(move || {
                    crate::quotas::apply_quota_routing(
                        enabled,
                        threshold,
                        repo.as_ref(),
                        &master_key,
                        eligible,
                        &req_model,
                    )
                })
                .await
                .unwrap_or_default()
            };
            if filtered.is_empty() {
                let err = CoreError::NoHealthyTargets(combo.id.0);
                return Ok(pipeline.failure(err, attempt - 1, ErrorPhase::Route));
            }

            state.eligible_targets = Some(filtered);
            inner.call(state).await
        })
    }
}

pub struct QuotaLayer {
    pub pipeline: Pipeline,
}

impl QuotaLayer {
    pub fn new(pipeline: Pipeline) -> Self {
        Self { pipeline }
    }
}

impl<S> tower::Layer<S> for QuotaLayer {
    type Service = QuotaService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        QuotaService::new(self.pipeline.clone(), inner)
    }
}

// =====================================================================
// Routing/Dispatch Service (Leaf)
// =====================================================================

#[derive(Clone)]
pub struct RoutingService {
    pub pipeline: Pipeline,
}

impl RoutingService {
    pub fn new(pipeline: Pipeline) -> Self {
        Self { pipeline }
    }
}

impl tower::Service<PipelineState> for RoutingService {
    type Response = PipelineResult;
    type Error = std::convert::Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, state: PipelineState) -> Self::Future {
        let pipeline = self.pipeline.clone();

        Box::pin(async move {
            let combo = state.combo.unwrap();
            let to_run = state.eligible_targets.unwrap();

            let race_size: usize = match combo.strategy {
                Strategy::Priority => to_run.len(),
                Strategy::RoundRobin | Strategy::Shuffle => (combo.race_size as usize)
                    .min(to_run.len())
                    .min(pipeline.config.racing.max_race_size as usize),
            };

            let mut last_result: Option<PipelineResult> = None;

            // 1. Parallel race path.
            if combo.race_size > 1 && to_run.len() >= 2 {
                let race_n = (combo.race_size as usize)
                    .min(to_run.len())
                    .min(pipeline.config.racing.max_race_size as usize);
                let race_result = crate::racing::run_race(
                    &pipeline,
                    state.req.clone(),
                    &combo,
                    to_run.clone(),
                    race_n as u8,
                )
                .await;

                if race_result.error.is_none() {
                    if let Some((request_id, attempt, target_id)) = race_result.usage_tuple.clone()
                    {
                        let job = crate::worker::BackgroundJob::MarkClientResponse {
                            request_id,
                            attempt,
                            target_id,
                        };
                        if let Err(e) = pipeline.config.background_tx.try_send(job)
                            && matches!(e, tokio::sync::mpsc::error::TrySendError::Closed(_))
                        {
                            let job = e.into_inner();
                            let conn = pipeline.conn.clone();
                            let repo = pipeline.repo().clone();
                            let sel = pipeline.selection_registry().clone();
                            let _ = tokio::task::spawn_blocking(move || {
                                crate::worker::process_job(&conn, repo.as_ref(), job, sel);
                            })
                            .await;
                        }
                    }
                    return Ok(race_result);
                }

                tracing::warn!(
                    combo_id = combo.id.0,
                    race_size = race_n,
                    total_targets = to_run.len(),
                    last_error = ?race_result.error,
                    "race exhausted all lanes; falling through to sequential targets"
                );
                last_result = Some(race_result);
            }

            // 2. Sequential execution.
            let attempt: u8 = 1;
            let mut combo_walk_log: Vec<String> = Vec::new();

            for (idx, target) in to_run.iter().enumerate() {
                let client_disconnected = {
                    let mut rx = state.req.client_disconnected.clone();
                    pipeline.is_client_disconnected(&mut rx)
                };
                if client_disconnected {
                    tracing::warn!(
                        combo_id = combo.id.0,
                        target_id = target.target.id.0,
                        provider = %target.target.provider_id,
                        attempt,
                        "client cancelled between targets; aborting pipeline"
                    );
                    return Ok(pipeline.client_disconnected_result(attempt));
                }

                let policy = crate::retry::RetryPolicy::from_config(&pipeline.config.retries);
                let mut target_attempt: u8 = 1;
                let mut result = pipeline
                    .execute_single(
                        state.req.clone(),
                        &combo,
                        target,
                        target_attempt,
                        race_size as u8,
                        &openproxy_adapters::upstream::CancellationToken::new(),
                    )
                    .await;

                while let Some(e) = &result.error {
                    if !crate::retry::RetryPolicy::is_retryable(
                        e,
                        pipeline.config.idle_chunk_retryable,
                    ) {
                        break;
                    }
                    if target_attempt >= policy.max_attempts {
                        break;
                    }
                    let client_disconnected = {
                        let mut rx = state.req.client_disconnected.clone();
                        pipeline.is_client_disconnected(&mut rx)
                    };
                    if client_disconnected {
                        break;
                    }
                    let delay = match policy.delay_after_attempt(target_attempt) {
                        Some(d) => d,
                        None => break,
                    };
                    let delay = if let CoreError::RateLimited {
                        retry_after_ms,
                        is_proxy_rotated,
                        ..
                    } = e
                    {
                        if *is_proxy_rotated {
                            std::time::Duration::from_millis(0)
                        } else {
                            let upstream = std::time::Duration::from_millis(*retry_after_ms);
                            if upstream > delay { upstream } else { delay }
                        }
                    } else {
                        delay
                    };
                    tracing::debug!(
                        combo_id = combo.id.0,
                        target_id = target.target.id.0,
                        provider = %target.target.provider_id,
                        target_attempt,
                        next_attempt = target_attempt + 1,
                        delay_ms = delay.as_millis() as u64,
                        error = %e,
                        "target failed retryably; retrying same target"
                    );
                    tokio::time::sleep(delay).await;
                    target_attempt = target_attempt.saturating_add(1);
                    result = pipeline
                        .execute_single(
                            state.req.clone(),
                            &combo,
                            target,
                            target_attempt,
                            race_size as u8,
                            &openproxy_adapters::upstream::CancellationToken::new(),
                        )
                        .await;
                }

                let cooldown_op = match &result.error {
                    None => Some("clear"),
                    Some(e) if crate::is_upstream_health_issue(e) => Some("record"),
                    Some(_) => None,
                };

                {
                    pipeline.selection_registry.record_request(target.target.id);
                    if result.error.is_none() {
                        pipeline.selection_registry.record_success(target.target.id);
                    }
                }

                if let Some(op) = cooldown_op {
                    match op {
                        "clear" => {
                            let repo = pipeline.repo().clone();
                            let target_id = target.target.id;
                            let combo_id = combo.id.0;
                            let _ = tokio::task::spawn_blocking(move || {
                                if let Err(e) = repo.clear_cooldown(target_id) {
                                    tracing::warn!(
                                        combo_id,
                                        target_id = target_id.0,
                                        error = %e,
                                        "cooldown::clear failed; non-fatal"
                                    );
                                }
                            })
                            .await;
                        }
                        "record" => {
                            let reason = result
                                .error
                                .as_ref()
                                .map(|e| e.to_string())
                                .unwrap_or_else(|| "retryable failure".to_string());
                            let mode = combo.cooldown_mode;
                            let base_secs = combo
                                .cooldown_base_secs
                                .unwrap_or(pipeline.config.cooldown_secs);
                            let max_secs = combo
                                .cooldown_max_secs
                                .unwrap_or(pipeline.config.cooldown_max_secs);
                            let factor = combo
                                .cooldown_factor
                                .unwrap_or(pipeline.config.cooldown_factor);
                            let is_rate_limited = matches!(
                                result.error,
                                Some(openproxy_types::error::CoreError::RateLimited { .. })
                            );
                            let repo = pipeline.repo().clone();
                            let target_id = target.target.id;
                            let account_id_opt = target.target.account_id;
                            let req_model = target.model.model_id.0.clone();
                            let combo_id = combo.id.0;
                            let _ = tokio::task::spawn_blocking(move || {
                                let mut final_base_secs = base_secs;
                                if is_rate_limited && let Some(account_id) = account_id_opt {
                                    let override_secs = (|| -> Option<u64> {
                                        let (accounts, _, _) =
                                            repo.get_accounts_meta(&[account_id]).ok()?;
                                        let account = accounts.get(&account_id.0)?;

                                        let mut reset_str = account.quota_session_reset_at.clone();

                                        if let Some(json) = &account.quota_model_details
                                            && let Ok(details) =
                                                serde_json::from_str::<
                                                    Vec<openproxy_types::quota::ModelQuotaDetail>,
                                                >(json)
                                        {
                                            let norm_req =
                                                openproxy_types::model_normalize::normalize_model_id(
                                                    &req_model,
                                                );

                                            if let Some(detail) = details.iter().find(|d| {
                                                let norm_detail =
                                                    openproxy_types::model_normalize::normalize_model_id(
                                                        &d.model_id,
                                                    );
                                                norm_req.eq_ignore_ascii_case(&norm_detail)
                                                    || req_model.eq_ignore_ascii_case(&d.model_id)
                                            }) && detail.session_reset_at.is_some()
                                            {
                                                reset_str = detail.session_reset_at.clone();
                                            }
                                        }

                                        openproxy_types::quota::parse_reset_time(&reset_str?)
                                    })();

                                    if let Some(secs) = override_secs
                                        && secs > final_base_secs
                                    {
                                        final_base_secs = secs;
                                    }
                                }

                                if let Err(e) = repo.record_cooldown(
                                    target_id,
                                    &reason,
                                    mode,
                                    final_base_secs,
                                    max_secs,
                                    factor,
                                ) {
                                    tracing::warn!(
                                        combo_id,
                                        target_id = target_id.0,
                                        error = %e,
                                        "cooldown::record failed; non-fatal"
                                    );
                                }
                            }).await;
                        }
                        _ => {}
                    }
                }

                let model_name = target
                    .target
                    .model_row_id
                    .map(|_| "unresolved")
                    .unwrap_or("unknown");
                let outcome = if let Some(err) = &result.error {
                    if crate::retry::RetryPolicy::is_retryable(
                        err,
                        pipeline.config.idle_chunk_retryable,
                    ) {
                        "retryable_failure"
                    } else {
                        "fatal_failure"
                    }
                } else {
                    "success"
                };
                combo_walk_log.push(format!(
                    "  [{}] {} (model: {}, id: {}): {} (attempts: {})",
                    idx + 1,
                    target.target.provider_id,
                    model_name,
                    target.target.id.0,
                    outcome,
                    target_attempt
                ));

                if result.error.is_none() {
                    if let Some((request_id, attempt, target_id)) = result.usage_tuple.clone() {
                        let job = crate::worker::BackgroundJob::MarkClientResponse {
                            request_id,
                            attempt,
                            target_id,
                        };
                        if let Err(e) = pipeline.config.background_tx.try_send(job)
                            && matches!(e, tokio::sync::mpsc::error::TrySendError::Closed(_))
                        {
                            let job = e.into_inner();
                            let conn = pipeline.conn.clone();
                            let repo = pipeline.repo().clone();
                            let sel = pipeline.selection_registry().clone();
                            let _ = tokio::task::spawn_blocking(move || {
                                crate::worker::process_job(&conn, repo.as_ref(), job, sel);
                            })
                            .await;
                        }
                    }
                    tracing::info!(
                        combo_id = combo.id.0,
                        combo_name = %combo.name,
                        walk = %combo_walk_log.join(" -> "),
                        "combo execution succeeded after walking targets sequentially"
                    );
                    return Ok(result);
                }

                last_result = Some(result);
            }

            tracing::error!(
                combo_id = combo.id.0,
                combo_name = %combo.name,
                walk = %combo_walk_log.join(" -> "),
                "combo execution failed after walking all targets sequentially"
            );

            Ok(last_result.unwrap_or_else(|| {
                pipeline.failure(
                    CoreError::NoHealthyTargets(combo.id.0),
                    attempt - 1,
                    ErrorPhase::Route,
                )
            }))
        })
    }
}

pub struct RoutingLayer {
    pub pipeline: Pipeline,
}

impl RoutingLayer {
    pub fn new(pipeline: Pipeline) -> Self {
        Self { pipeline }
    }
}

impl<S> tower::Layer<S> for RoutingLayer {
    type Service = RoutingService;

    fn layer(&self, _inner: S) -> Self::Service {
        RoutingService::new(self.pipeline.clone())
    }
}

#[cfg(test)]
mod tests;
