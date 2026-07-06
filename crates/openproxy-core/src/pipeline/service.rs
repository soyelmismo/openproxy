use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use crate::circuit_breaker::Health;
use crate::combos::{Combo, ComboTarget, Strategy};
use crate::error::CoreError;
use crate::pipeline::ErrorPhase;
use crate::pipeline::{Pipeline, PipelineRequest, PipelineResult};
use crate::pipeline::repository::PipelineRepository;

/// Request state passed down the middleware chain.
pub struct PipelineState {
    pub req: PipelineRequest,
    pub combo: Option<Combo>,
    pub eligible_targets: Option<Vec<ComboTarget>>,
    pub race_size: Option<usize>,
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
    S: tower::Service<PipelineState, Response = PipelineResult, Error = std::convert::Infallible> + Clone + Send + 'static,
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
            let combo = match pipeline.load_combo(&state.req) {
                Ok(c) => c,
                Err(e) => return Ok(pipeline.failure(e, 0, ErrorPhase::Resolve)),
            };

            let attempt: u8 = 1;

            // 2. Resolve and expand targets.
            let targets = match pipeline.resolve_targets(&combo, state.req.targets_override.as_deref()) {
                Ok(t) => t,
                Err(e) => return Ok(pipeline.failure(e, attempt - 1, ErrorPhase::Resolve)),
            };

            // 3. Flatten sub-combos.
            let flat_targets = match pipeline.flatten_targets(&combo.id, targets.clone()) {
                Ok(t) => t,
                Err(e) => return Ok(pipeline.failure(e, attempt - 1, ErrorPhase::Resolve)),
            };

            // 4. Filter out accounts that the circuit breaker marks unhealthy.
            let pre_cb_snapshot: Vec<ComboTarget> = flat_targets.clone();
            let mut eligible: Vec<ComboTarget> = flat_targets
                .into_iter()
                .filter(|t| match t.account_id {
                    Some(aid) => pipeline.circuit_breaker.is_healthy(aid) == Health::Healthy,
                    None => true,
                })
                .collect();

            if eligible.is_empty() && !pre_cb_snapshot.is_empty() {
                tracing::warn!(
                    combo_id = combo.id.0,
                    parked = pre_cb_snapshot.len(),
                    "all targets' accounts unhealthy in circuit_breaker; falling through to pre-CB dispatch"
                );
                eligible = pre_cb_snapshot;
            }

            if eligible.is_empty() {
                if attempt == 1 {
                    let repopulated = match pipeline.repo().auto_populate_empty_combo(combo.id) {
                        Ok(n) => n,
                        Err(e) => {
                            tracing::warn!(
                                combo_id = combo.id.0,
                                combo_name = %combo.name,
                                error = %e,
                                "auto_populate on NoHealthyTargets failed; recording failure"
                            );
                            let started = std::time::Instant::now();
                            let _ = pipeline.repo().record_no_healthy_targets_row(
                                &state.req.request_id.to_string(),
                                &state.req.trace_id.to_string(),
                                &combo,
                                started.elapsed().as_millis() as u64,
                                &chrono::Utc::now().naive_utc().to_string(),
                                "No healthy targets available"
                            );
                            return Ok(pipeline.failure(e, attempt - 1, ErrorPhase::Route));
                        }
                    };
                    if repopulated > 0 {
                        let targets = match pipeline.resolve_targets(&combo, state.req.targets_override.as_deref()) {
                            Ok(t) => t,
                            Err(e) => return Ok(pipeline.failure(e, attempt - 1, ErrorPhase::Resolve)),
                        };
                        let flat_targets = match pipeline.flatten_targets(&combo.id, targets) {
                            Ok(t) => t,
                            Err(e) => return Ok(pipeline.failure(e, attempt - 1, ErrorPhase::Resolve)),
                        };
                        let re_eligible: Vec<ComboTarget> = flat_targets
                            .into_iter()
                            .filter(|t| match t.account_id {
                                Some(aid) => pipeline.circuit_breaker.is_healthy(aid) == Health::Healthy,
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
                    let started = std::time::Instant::now();
                    let _ = pipeline.repo().record_no_healthy_targets_row(
                        &state.req.request_id.to_string(),
                        &state.req.trace_id.to_string(),
                        &combo,
                        started.elapsed().as_millis() as u64,
                        &chrono::Utc::now().naive_utc().to_string(),
                        "No healthy targets available"
                    );
                    return Ok(pipeline.failure(err, attempt - 1, ErrorPhase::Route));
                }
            }

            state.combo = Some(combo);
            state.eligible_targets = Some(eligible);

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
    S: tower::Service<PipelineState, Response = PipelineResult, Error = std::convert::Infallible> + Clone + Send + 'static,
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
            let mut eligible = state.eligible_targets.take().unwrap();
            let attempt: u8 = 1;

            // Apply dynamic quota routing and protection.
            eligible = pipeline.apply_quota_routing(eligible, &state.req.openai_request.model);
            if eligible.is_empty() {
                let err = CoreError::NoHealthyTargets(combo.id.0);
                let started = std::time::Instant::now();
                let _ = pipeline.repo().record_no_healthy_targets_row(
                    &state.req.request_id.to_string(),
                    &state.req.trace_id.to_string(),
                    combo,
                    started.elapsed().as_millis() as u64,
                    &chrono::Utc::now().naive_utc().to_string(),
                    "No healthy targets available"
                );
                return Ok(pipeline.failure(err, attempt - 1, ErrorPhase::Route));
            }

            state.eligible_targets = Some(eligible);
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
                let race_result = pipeline.run_race(&state.req, &combo, to_run.clone(), race_n as u8).await;

                if race_result.error.is_none() {
                    if let Some(row_id) = race_result.usage_row_id {
                        let _ = pipeline.repo().mark_client_response(row_id);
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

            for target in to_run.iter() {
                let client_disconnected = {
                    let mut rx = state.req.client_disconnected.clone();
                    pipeline.is_client_disconnected(&mut rx)
                };
                if client_disconnected {
                    tracing::warn!(
                        combo_id = combo.id.0,
                        target_id = target.id.0,
                        provider = %target.provider_id,
                        attempt,
                        "client cancelled between targets; aborting pipeline"
                    );
                    return Ok(pipeline.client_disconnected_result(attempt));
                }

                let policy = crate::retry::RetryPolicy::from_config(&pipeline.config.retries);
                let mut target_attempt: u8 = 1;
                let mut result = pipeline
                    .execute_single(
                        &state.req,
                        &combo,
                        target,
                        target_attempt,
                        race_size as u8,
                        &crate::upstream::CancellationToken::new(),
                    )
                    .await;

                while let Some(e) = &result.error {
                    if !crate::retry::RetryPolicy::is_retryable(e, pipeline.config.idle_chunk_retryable) {
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
                    let delay = if let CoreError::RateLimited { retry_after_ms, .. } = e {
                        let upstream = std::time::Duration::from_millis(*retry_after_ms);
                        if upstream > delay { upstream } else { delay }
                    } else {
                        delay
                    };
                    tracing::debug!(
                        combo_id = combo.id.0,
                        target_id = target.id.0,
                        provider = %target.provider_id,
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
                            &state.req,
                            &combo,
                            target,
                            target_attempt,
                            race_size as u8,
                            &crate::upstream::CancellationToken::new(),
                        )
                        .await;
                }

                let cooldown_op = match &result.error {
                    None => Some("clear"),
                    Some(e) if crate::pipeline::is_upstream_health_issue(e) => Some("record"),
                    Some(_) => None,
                };

                {
                    pipeline.selection_registry.record_request(target.id);
                    if result.error.is_none() {
                        pipeline.selection_registry.record_success(target.id);
                    }
                }

                if let Some(op) = cooldown_op {
                    match op {
                        "clear" => {
                            if let Err(e) = pipeline.repo().clear_cooldown(target.id) {
                                tracing::warn!(
                                    combo_id = combo.id.0,
                                    target_id = target.id.0,
                                    error = %e,
                                    "cooldown::clear failed; non-fatal"
                                );
                            }
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
                            let factor = combo.cooldown_factor.unwrap_or(pipeline.config.cooldown_factor);
                            if let Err(e) = pipeline.repo().record_cooldown(
                                target.id,
                                &reason,
                                mode,
                                base_secs,
                                max_secs,
                                factor,
                            ) {
                                tracing::warn!(
                                    combo_id = combo.id.0,
                                    target_id = target.id.0,
                                    error = %e,
                                    "cooldown::record failed; non-fatal"
                                );
                            }
                        }
                        _ => {}
                    }
                }

                let model_name = match target.model_row_id {
                    Some(row_id) => match pipeline.repo().load_model(row_id) {
                        Ok(m) => m.model_id.as_str().to_string(),
                        Err(_) => format!("row_id={}", row_id.0),
                    },
                    None => "sub_combo".to_string(),
                };
                let outcome = match &result.error {
                    None => "OK".to_string(),
                    Some(err) => format!("ERR({})", err.http_status()),
                };
                combo_walk_log.push(format!(
                    "{}:{}/{}={}",
                    target.provider_id, model_name, target.id.0, outcome
                ));

                if result.error.is_none() {
                    if let Some(row_id) = result.usage_row_id {
                        let _ = pipeline.repo().mark_client_response(row_id);
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
