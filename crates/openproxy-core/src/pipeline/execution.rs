use crate::adapters::{AdapterFormat, ProviderAdapter};
use crate::circuit_breaker::Health;
use crate::combos::{self, Combo, ComboTarget, Strategy};
use crate::compression::{CompressionMode, stats::CompressionStats};
use crate::error::{CoreError, Result};
use crate::cost::{self, UsageInput};
use crate::ids::{AccountId, ApiKeyId, ComboId, ModelRowId, TraceId, UsageId};
use crate::models::{self, Model};
use crate::pipeline::{FailureContext, ErrorPhase, Pipeline, PipelineRequest, PipelineResult, QuotaStatus, parse_retry_after_ms, is_upstream_health_issue};
use crate::retry::RetryPolicy;
use crate::think_extractor::extract_think_from_response;
use crate::timeouts::{self, ModelTimeoutOverrides, Timeouts};
use crate::translation::OpenAIResponse;
use crate::upstream::{
    CancellationToken, UpstreamError, UpstreamPhase, UpstreamRequest,
};
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::watch;

impl Pipeline {
    /// Drive one chat-completion request to completion.
    pub async fn run(&self, req: PipelineRequest) -> PipelineResult {
        let combo = match self.load_combo(&req) {
            Ok(c) => c,
            Err(e) => return self.failure(e, 0, ErrorPhase::Resolve),
        };

        let attempt: u8 = 1;
        let targets = match self.resolve_targets(&combo, req.targets_override.as_deref()) {
            Ok(t) => t,
            Err(e) => return self.failure(e, attempt - 1, ErrorPhase::Resolve),
        };

        let flat_targets = match self.flatten_targets(&combo.id, targets.clone()) {
            Ok(t) => t,
            Err(e) => return self.failure(e, attempt - 1, ErrorPhase::Resolve),
        };

        let pre_cb_snapshot: Vec<ComboTarget> = flat_targets.clone();
        let mut eligible: Vec<ComboTarget> = flat_targets
            .into_iter()
            .filter(|t| match t.account_id {
                Some(aid) => self.circuit_breaker.is_healthy(aid) == Health::Healthy,
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
                let repopulated = match self.auto_populate_if_empty(&combo) {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::warn!(
                            combo_id = combo.id.0,
                            combo_name = %combo.name,
                            error = %e,
                            "auto_populate on NoHealthyTargets failed; recording failure"
                        );
                        let started = std::time::Instant::now();
                        self.record_no_healthy_targets_row(&req, &combo, started);
                        return self.failure(e, attempt - 1, ErrorPhase::Route);
                    }
                };
                if repopulated > 0 {
                    let targets =
                        match self.resolve_targets(&combo, req.targets_override.as_deref()) {
                            Ok(t) => t,
                            Err(e) => return self.failure(e, attempt - 1, ErrorPhase::Resolve),
                        };
                    let flat_targets = match self.flatten_targets(&combo.id, targets) {
                        Ok(t) => t,
                        Err(e) => return self.failure(e, attempt - 1, ErrorPhase::Resolve),
                    };
                    let re_eligible: Vec<ComboTarget> = flat_targets
                        .into_iter()
                        .filter(|t| match t.account_id {
                            Some(aid) => self.circuit_breaker.is_healthy(aid) == Health::Healthy,
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
                self.record_no_healthy_targets_row(&req, &combo, started);
                return self.failure(err, attempt - 1, ErrorPhase::Route);
            }
        }

        eligible = self.apply_quota_routing(eligible, &req.openai_request.model);
        if eligible.is_empty() {
            let err = CoreError::NoHealthyTargets(combo.id.0);
            let started = std::time::Instant::now();
            self.record_no_healthy_targets_row(&req, &combo, started);
            return self.failure(err, attempt - 1, ErrorPhase::Route);
        }

        let race_size: usize = match combo.strategy {
            Strategy::Priority => eligible.len(),
            Strategy::RoundRobin | Strategy::Shuffle => (combo.race_size as usize)
                .min(eligible.len())
                .min(self.config.racing.max_race_size as usize),
        };
        let to_run: Vec<ComboTarget> = eligible;
        let to_run_unfiltered_snapshot: Vec<ComboTarget> = to_run.clone();
        let mut to_run: Vec<ComboTarget> = to_run;
        let to_run_unfiltered: Vec<ComboTarget> = to_run_unfiltered_snapshot;

        if to_run.is_empty() {
            if to_run_unfiltered.is_empty() {
                let err = CoreError::NoHealthyTargets(combo.id.0);
                let started = std::time::Instant::now();
                self.record_no_healthy_targets_row(&req, &combo, started);
                return self.failure(err, attempt - 1, ErrorPhase::Route);
            }
            tracing::warn!(
                combo_id = combo.id.0,
                parked = to_run_unfiltered.len(),
                "all targets in cooldown for this request; falling through to unfiltered dispatch"
            );
            to_run = to_run_unfiltered;
        }

        let mut last_result: Option<PipelineResult> = None;

        if combo.race_size > 1 && to_run.len() >= 2 {
            let race_n = (combo.race_size as usize)
                .min(to_run.len())
                .min(self.config.racing.max_race_size as usize);
            let race_result = self.run_race(&req, &combo, to_run.clone(), race_n as u8).await;

            if race_result.error.is_none() {
                self.mark_client_response(race_result.usage_row_id);
                return race_result;
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

        let mut combo_walk_log: Vec<String> = Vec::new();

        for target in to_run.iter() {
            let client_disconnected = {
                let mut rx = req.client_disconnected.clone();
                self.is_client_disconnected(&mut rx)
            };
            if client_disconnected {
                tracing::warn!(
                    combo_id = combo.id.0,
                    target_id = target.id.0,
                    provider = %target.provider_id,
                    attempt,
                    "client cancelled between targets; aborting pipeline"
                );
                return self.client_disconnected_result(attempt);
            }

            let policy = RetryPolicy::from_config(&self.config.retries);
            let mut target_attempt: u8 = 1;
            let mut result = self
                .execute_single(
                    &req,
                    &combo,
                    target,
                    target_attempt,
                    race_size as u8,
                    &CancellationToken::new(),
                )
                .await;

            while let Some(e) = &result.error {
                if !RetryPolicy::is_retryable(e, self.config.idle_chunk_retryable) {
                    break;
                }
                if target_attempt >= policy.max_attempts {
                    break;
                }
                let client_disconnected = {
                    let mut rx = req.client_disconnected.clone();
                    self.is_client_disconnected(&mut rx)
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
                result = self
                    .execute_single(
                        &req,
                        &combo,
                        target,
                        target_attempt,
                        race_size as u8,
                        &CancellationToken::new(),
                    )
                    .await;
            }

            let cooldown_op = match &result.error {
                None => Some("clear"),
                Some(e) if is_upstream_health_issue(e) => Some("record"),
                Some(_) => None,
            };

            {
                self.selection_registry.record_request(target.id);
                if result.error.is_none() {
                    self.selection_registry.record_success(target.id);
                }
            }
            if cooldown_op.is_some() {
                let cooldown_conn = self.conn.lock();
                match cooldown_op {
                    Some("clear") => {
                        if let Err(e) = crate::cooldown::clear(&cooldown_conn, target.id) {
                            tracing::warn!(
                                combo_id = combo.id.0,
                                target_id = target.id.0,
                                error = %e,
                                "cooldown::clear failed; non-fatal"
                            );
                        }
                    }
                    Some("record") => {
                        let reason = result
                            .error
                            .as_ref()
                            .map(|e| e.to_string())
                            .unwrap_or_else(|| "retryable failure".to_string());
                        let mode = combo.cooldown_mode;
                        let base_secs = combo
                            .cooldown_base_secs
                            .unwrap_or(self.config.cooldown_secs);
                        let max_secs = combo
                            .cooldown_max_secs
                            .unwrap_or(self.config.cooldown_max_secs);
                        let factor = combo.cooldown_factor.unwrap_or(self.config.cooldown_factor);
                        if let Err(e) = crate::cooldown::record_failure_with_mode(
                            &cooldown_conn,
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
                                "cooldown::record_failure_with_mode failed; non-fatal"
                            );
                        }
                    }
                    _ => unreachable!(),
                }
            }
            match &result.error {
                None => {
                    self.mark_client_response(result.usage_row_id);
                    return result;
                }
                Some(e) => {
                    let is_rate_limit = matches!(e, CoreError::RateLimited { .. })
                        || (matches!(e, CoreError::UpstreamError { status, .. } if *status == 429));
                    if is_rate_limit {
                        tracing::warn!(
                            combo_id = combo.id.0,
                            target_id = target.id.0,
                            provider = %target.provider_id,
                            model_row_id = ?target.model_row_id,
                            attempt = target_attempt,
                            retryable = RetryPolicy::is_retryable(e, self.config.idle_chunk_retryable),
                            error = %e,
                            remaining_targets = to_run.len(),
                            "target rate-limited; trying next target in combo"
                        );
                    } else {
                        tracing::debug!(
                            combo_id = combo.id.0,
                            target_id = target.id.0,
                            provider = %target.provider_id,
                            strategy = ?combo.strategy,
                            retryable = RetryPolicy::is_retryable(e, self.config.idle_chunk_retryable),
                            error = %e,
                            "target failed; trying next target"
                        );
                    }
                    combo_walk_log.push(format!(
                        "  target_id={} provider={} attempts={} error={}",
                        target.id.0,
                        target.provider_id,
                        target_attempt,
                        e
                    ));
                    last_result = Some(result);
                }
            }
        }

        if let Some(ref r) = last_result
            && r.error.is_some()
        {
            tracing::warn!(
                combo_id = combo.id.0,
                total_targets = to_run.len(),
                targets_tried = combo_walk_log.len(),
                last_error = ?r.error,
                "combo exhausted: all {} target(s) failed, returning last error to client.\nCombo walk summary:\n{}",
                combo_walk_log.len(),
                combo_walk_log.join("\n")
            );
            self.mark_client_response(r.usage_row_id);
        }
        last_result.unwrap_or_else(|| {
            self.failure(
                CoreError::NoHealthyTargets(combo.id.0),
                attempt,
                ErrorPhase::Route,
            )
        })
    }

    pub(crate) fn flatten_targets(
        &self,
        root_combo_id: &ComboId,
        targets: Vec<ComboTarget>,
    ) -> Result<Vec<ComboTarget>> {
        if !targets.iter().any(|t| t.sub_combo_id.is_some()) {
            return Ok(targets);
        }
        let mut out = Vec::with_capacity(targets.len());
        let conn = self.conn.lock();
        let mut visited: Vec<ComboId> = vec![*root_combo_id];
        for t in targets {
            if let Some(sub_id) = t.sub_combo_id {
                let sub_flat = combos::resolve_combo_to_targets(&conn, sub_id, &mut visited, 0)?;
                out.extend(sub_flat);
            } else {
                out.push(t);
            }
        }
        let expanded = combos::expand_account_rotation(&conn, out)?;
        Ok(expanded)
    }

    pub(crate) fn evaluate_account_quota(
        &self,
        account: &crate::accounts::Account,
        requested_model: &str,
    ) -> QuotaStatus {
        let quota_protection_enabled = self.config.quota_protection.enabled;
        let threshold_percentage = self.config.quota_protection.threshold_percentage;

        if let (Some(used), Some(limit)) = (account.quota_session_used, account.quota_session_limit) {
            if used >= limit {
                return QuotaStatus::Exhausted;
            }
        }
        if let (Some(used), Some(limit)) = (account.quota_weekly_used, account.quota_weekly_limit) {
            if used >= limit {
                return QuotaStatus::Exhausted;
            }
        }

        if let Some(ref details_val) = account.quota_model_details {
            if let Ok(details) = serde_json::from_value::<Vec<crate::quota::ModelQuotaDetail>>(details_val.clone()) {
                let norm_req = crate::model_normalize::normalize_model_id(requested_model);

                for detail in details {
                    let norm_detail = crate::model_normalize::normalize_model_id(&detail.model_id);

                    let is_match = norm_req.to_lowercase() == norm_detail.to_lowercase()
                        || requested_model.to_lowercase() == detail.model_id.to_lowercase();

                    if is_match {
                        if detail.remaining_fraction <= 0.0 {
                            return QuotaStatus::Exhausted;
                        }
                        if quota_protection_enabled {
                            let threshold_fraction = (threshold_percentage as f64) / 100.0;
                            if detail.remaining_fraction <= threshold_fraction {
                                return QuotaStatus::Protected;
                            }
                        }
                        break;
                    }
                }
            }
        }

        QuotaStatus::Available
    }

    pub(crate) fn get_account_remaining_fraction(
        &self,
        account: &crate::accounts::Account,
        requested_model: &str,
    ) -> f64 {
        if let Some(ref details_val) = account.quota_model_details {
            if let Ok(details) = serde_json::from_value::<Vec<crate::quota::ModelQuotaDetail>>(details_val.clone()) {
                let norm_req = crate::model_normalize::normalize_model_id(requested_model);

                for detail in details {
                    let norm_detail = crate::model_normalize::normalize_model_id(&detail.model_id);

                    let is_match = norm_req.to_lowercase() == norm_detail.to_lowercase()
                        || requested_model.to_lowercase() == detail.model_id.to_lowercase();

                    if is_match {
                        return detail.remaining_fraction;
                    }
                }
            }
        }

        if let (Some(used), Some(limit)) = (account.quota_session_used, account.quota_session_limit) {
            if limit > 0 {
                return (limit.saturating_sub(used) as f64) / (limit as f64);
            }
        }

        if let (Some(used), Some(limit)) = (account.quota_weekly_used, account.quota_weekly_limit) {
            if limit > 0 {
                return (limit.saturating_sub(used) as f64) / (limit as f64);
            }
        }

        1.0
    }

    pub(crate) fn apply_quota_routing(
        &self,
        targets: Vec<ComboTarget>,
        requested_model: &str,
    ) -> Vec<ComboTarget> {
        let conn = self.conn.lock();

        struct TargetWithQuota {
            target: ComboTarget,
            status: QuotaStatus,
            remaining_fraction: f64,
            priority: i32,
        }

        let mut processed_targets = Vec::with_capacity(targets.len());

        for t in targets {
            let Some(aid) = t.account_id else {
                processed_targets.push(TargetWithQuota {
                    target: t,
                    status: QuotaStatus::Available,
                    remaining_fraction: 1.0,
                    priority: 0,
                });
                continue;
            };

            match crate::accounts::get(&conn, aid) {
                Ok(Some(account)) => {
                    let status = self.evaluate_account_quota(&account, requested_model);
                    let remaining_fraction = self.get_account_remaining_fraction(&account, requested_model);
                    processed_targets.push(TargetWithQuota {
                        target: t,
                        status,
                        remaining_fraction,
                        priority: account.priority,
                    });
                }
                _ => {
                    processed_targets.push(TargetWithQuota {
                        target: t,
                        status: QuotaStatus::Available,
                        remaining_fraction: 1.0,
                        priority: 0,
                    });
                }
            }
        }

        let non_exhausted: Vec<TargetWithQuota> = processed_targets
            .into_iter()
            .filter(|t| t.status != QuotaStatus::Exhausted)
            .collect();

        let has_available = non_exhausted.iter().any(|t| t.status == QuotaStatus::Available);

        let mut final_targets: Vec<TargetWithQuota> = if has_available {
            non_exhausted
                .into_iter()
                .filter(|t| t.status == QuotaStatus::Available)
                .collect()
        } else {
            non_exhausted
        };

        final_targets.sort_by(|a, b| {
            let pri_cmp = a.priority.cmp(&b.priority);
            if pri_cmp != std::cmp::Ordering::Equal {
                return pri_cmp;
            }

            let quota_cmp = b.remaining_fraction.partial_cmp(&a.remaining_fraction).unwrap_or(std::cmp::Ordering::Equal);
            if quota_cmp != std::cmp::Ordering::Equal {
                return quota_cmp;
            }

            a.target.priority_order.cmp(&b.target.priority_order)
        });

        final_targets.into_iter().map(|t| t.target).collect()
    }

    fn auto_populate_if_empty(&self, combo: &Combo) -> Result<usize> {
        {
            let conn = self.conn.lock();
            if !combos::list_targets(&conn, combo.id)?.is_empty() {
                return Ok(0);
            }
        }

        let added = {
            let conn = self.conn.lock();
            combos::auto_populate_empty_combo(&conn, combo.id)?
        };

        if added > 0 {
            tracing::info!(
                combo_id = combo.id.0,
                combo_name = %combo.name,
                added_targets = added,
                "auto-populated empty combo with healthy provider's active models"
            );
        }
        Ok(added)
    }

    fn record_no_healthy_targets_row(
        &self,
        req: &PipelineRequest,
        combo: &Combo,
        started: Instant,
    ) {
        let input = UsageInput {
            request_id: req.request_id,
            trace_id: req.trace_id.to_string(),
            attempt: 1,
            provider_id: crate::ids::ProviderId::new(""),
            account_id: None,
            combo_id: Some(combo.id),
            combo_target_id: None,
            model_row_id: None,
            upstream_model_id: req.openai_request.model.clone(),
            prompt_tokens: None,
            completion_tokens: None,
            connect_ms: None,
            ttft_ms: None,
            total_ms: started.elapsed().as_millis() as u64,
            status_code: 502,
            error_msg: Some("no_healthy_targets".to_string()),
            race_total: 1,
            race_lost: false,
            api_key_id: req.api_key_id,
            request_body_json: None,
            response_body_json: None,
            request_headers: None,
            response_headers: None,
            error_message: Some("no_healthy_targets".to_string()),
            race_attempts: 1,
            is_streaming: false,
            stream_complete: false,
            stop_reason: None,
            compression_savings_pct: None,
            compression_techniques: None,
            client_response: true,
            prompt_tokens_estimated: false,
            completion_tokens_estimated: false,
            endpoint_kind: crate::endpoint::EndpointKind::Chat,
        };
        let conn = self.conn.lock();
        let _ = crate::cost::record(&conn, &input);
    }

    pub(crate) async fn run_race(
        &self,
        req: &PipelineRequest,
        combo: &Combo,
        to_run: Vec<ComboTarget>,
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
                usage_row_id: None,
            };
        }

        let queue: Arc<parking_lot::Mutex<VecDeque<ComboTarget>>> =
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
                    usage_row_id: None,
                };
            }
        };

        let (race_sink, worker_tokens) =
            crate::race_sink::RaceSink::new(original_tx, num_workers as usize);

        for worker_idx in 0..num_workers as usize {
            let p = self.clone();
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
                        .expect("run_race: worker must have race_cancel");
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

                    let result = p
                        .execute_single(&req, &combo, &target, 1, race_size, worker_token)
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
                        std::time::Duration::from_millis(self.config.racing.abort_grace_ms.max(50));
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
                    usage_row_id: None,
                };
            }
            all_done.notified().await;
        }
    }

    fn normalize_and_serialize(
        &self,
        value: &impl serde::Serialize,
        adapter: &dyn crate::adapters::ProviderAdapter,
        label: &str,
        req: &PipelineRequest,
        combo: &Combo,
        target: &ComboTarget,
        attempt: u8,
        race_size: u8,
        started: Instant,
        model: &Model,
    ) -> std::result::Result<bytes::Bytes, PipelineResult> {
        if adapter.needs_normalization() {
            let mut body_value = match serde_json::to_value(value) {
                Ok(v) => v,
                Err(e) => {
                    let err = CoreError::Parse(format!("serialize {label} to value: {e}"));
                    return Err(self.record_and_fail(
                        req,
                        combo,
                        target,
                        FailureContext {
                            attempt,
                            race_size,
                            err: &err,
                            started,
                            model: Some(model),
                            connect_ms: None,
                            ttft_ms: None,
                            status_code: 0,
                        },
                    ));
                }
            };
            adapter.normalize_request_body(&mut body_value);
            match serde_json::to_vec(&body_value) {
                Ok(v) => Ok(bytes::Bytes::from(v)),
                Err(e) => {
                    let err = CoreError::Parse(format!("serialize {label}: {e}"));
                    Err(self.record_and_fail(
                        req,
                        combo,
                        target,
                        FailureContext {
                            attempt,
                            race_size,
                            err: &err,
                            started,
                            model: Some(model),
                            connect_ms: None,
                            ttft_ms: None,
                            status_code: 0,
                        },
                    ))
                }
            }
        } else {
            match serde_json::to_vec(value) {
                Ok(v) => Ok(bytes::Bytes::from(v)),
                Err(e) => {
                    let err = CoreError::Parse(format!("serialize {label}: {e}"));
                    Err(self.record_and_fail(
                        req,
                        combo,
                        target,
                        FailureContext {
                            attempt,
                            race_size,
                            err: &err,
                            started,
                            model: Some(model),
                            connect_ms: None,
                            ttft_ms: None,
                            status_code: 0,
                        },
                    ))
                }
            }
        }
    }

    pub(crate) async fn execute_single(
        &self,
        req: &PipelineRequest,
        combo: &Combo,
        target: &ComboTarget,
        attempt: u8,
        race_size: u8,
        race_cancel: &CancellationToken,
    ) -> PipelineResult {
        let started = Instant::now();
        let trace_id = if attempt > 1 {
            format!("{}:retry{}", req.trace_id, attempt - 1)
        } else {
            req.trace_id.to_string()
        };

        if race_cancel.is_cancelled() {
            return self.record_and_fail_with_trace_id(
                req,
                combo,
                target,
                FailureContext {
                    attempt,
                    race_size,
                    err: &CoreError::RaceLost,
                    started,
                    model: None,
                    connect_ms: None,
                    ttft_ms: None,
                    status_code: CoreError::RaceLost.http_status(),
                },
                trace_id,
            );
        }

        let model_row_id = match target.model_row_id {
            Some(m) => m,
            None => {
                let err = CoreError::Internal(format!(
                    "execute_single called on a sub-combo target (id={})",
                    target.id.0
                ));
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    FailureContext {
                        attempt,
                        race_size,
                        err: &err,
                        started,
                        model: None,
                        connect_ms: None,
                        ttft_ms: None,
                        status_code: 0,
                    },
                );
            }
        };
        let model = match self.load_model(model_row_id) {
            Ok(m) => m,
            Err(e) => {
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    FailureContext {
                        attempt,
                        race_size,
                        err: &e,
                        started,
                        model: None,
                        connect_ms: None,
                        ttft_ms: None,
                        status_code: 0,
                    },
                );
            }
        };

        crate::usage::publish_stage_event(crate::usage::StageEvent {
            request_id: req.request_id.to_string(),
            trace_id: trace_id.to_string(),
            provider_id: target.provider_id.to_string(),
            upstream_model_id: model.model_id.as_str().to_string(),
            stage: "started".into(),
            elapsed_ms: 0,
            connect_ms: None,
            ttft_ms: None,
            status_code: 0,
            error: None,
            stop_reason: None,
            compression_savings_pct: None,
            compression_techniques: None,
            timestamp: String::new(),
            endpoint_kind: crate::endpoint::EndpointKind::Chat,
        });

        if let Some(account_id) = target.account_id {
            let is_custom = matches!(
                target.provider_id.as_str(),
                "kiro" | "antigravity"
            );
            if is_custom {
                let (mut access_token, kiro_meta, antigravity_project, maybe_refresh) = {
                    let conn = self.conn.lock();
                    let access_token = match crate::accounts::decrypt_access_token(
                        &conn,
                        account_id,
                        &self.config.master_key,
                    ) {
                        Ok(t) => t,
                        Err(e) => {
                            drop(conn);
                            return self.record_and_fail(
                                req,
                                combo,
                                target,
                                FailureContext {
                                    attempt,
                                    race_size,
                                    err: &e,
                                    started,
                                    model: Some(&model),
                                    connect_ms: None,
                                    ttft_ms: None,
                                    status_code: e.http_status(),
                                },
                            );
                        }
                    };

                    let maybe_refresh: Option<String> =
                        if self.config.oauth_provider_registry.is_some() {
                            let expires_at: Option<String> = conn
                                .query_row(
                                    "SELECT expires_at FROM accounts WHERE id = ?1",
                                    rusqlite::params![account_id.0],
                                    |row| row.get(0),
                                )
                                .ok()
                                .flatten();
                            if crate::oauth::pipeline_token_needs_refresh(
                                expires_at.as_deref(),
                                target.provider_id.as_str(),
                            ) {
                                match crate::accounts::decrypt_refresh_token(
                                    &conn,
                                    account_id,
                                    &self.config.master_key,
                                ) {
                                    Ok(Some(rt)) => Some(rt),
                                    _ => None,
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        };

                    let (token, meta, pid) = match target.provider_id.as_str() {
                        "kiro" => {
                            let m = crate::executor_kiro::read_account_meta(&conn, account_id)
                                .unwrap_or(None);
                            (access_token, m, None)
                        }
                        "antigravity" => {
                            let p = crate::executor_antigravity::read_project_id(&conn, account_id);
                            match p {
                                Ok(p) => (access_token, None, Some(p)),
                                Err(e) => {
                                    drop(conn);
                                    return self.record_and_fail(
                                        req,
                                        combo,
                                        target,
                                        FailureContext {
                                            attempt,
                                            race_size,
                                            err: &e,
                                            started,
                                            model: Some(&model),
                                            connect_ms: None,
                                            ttft_ms: None,
                                            status_code: e.http_status(),
                                        },
                                    );
                                }
                            }
                        }
                        _ => unreachable!(),
                    };
                    (token, meta, pid, maybe_refresh)
                };

                if let Some(refresh_token) = maybe_refresh
                    && let Some(registry) = self.config.oauth_provider_registry.as_ref()
                    && let Some(provider) = registry.get(target.provider_id.as_str())
                {
                    let provider_id_str = target.provider_id.as_str();
                    tracing::info!(
                        account = account_id.0,
                        provider = provider_id_str,
                        "pipeline: proactive OAuth token refresh"
                    );
                    match provider
                        .refresh_token(
                            &refresh_token,
                            &self.config.upstream_client,
                            account_id,
                            crate::oauth::DbRef::Connection(&self.conn),
                        )
                        .await
                    {
                        Ok(token) => {
                            let expires_at = token.expires_in.map(|secs| {
                                (chrono::Utc::now() + chrono::Duration::seconds(secs as i64))
                                    .format("%Y-%m-%dT%H:%M:%SZ")
                                    .to_string()
                            });
                            {
                                let conn = self.conn.lock();
                                let _ = crate::accounts::store_oauth_tokens(
                                    &conn,
                                    account_id,
                                    &token.access_token,
                                    token.refresh_token.as_deref(),
                                    &self.config.master_key,
                                    &token.token_type,
                                    expires_at.as_deref(),
                                    token.scope.as_deref(),
                                    None,
                                    None,
                                );
                            }
                            access_token = token.access_token;
                        }
                        Err(e) => {
                            tracing::warn!(
                                account = account_id.0,
                                provider = provider_id_str,
                                error = %e,
                                "pipeline: proactive OAuth refresh failed, \
                                 continuing with existing token"
                            );
                        }
                    }
                }

                let mut custom_req = req.openai_request.clone();
                custom_req.model = model.model_id.as_str().to_string();

                let executor_result = match target.provider_id.as_str() {
                    "kiro" => {
                        let region = kiro_meta
                            .as_ref()
                            .map(|m| m.region.as_str())
                            .filter(|r| !r.is_empty())
                            .unwrap_or(crate::executor_kiro::KIRO_DEFAULT_REGION);
                        let profile_arn = kiro_meta.as_ref().and_then(|m| m.profile_arn.as_deref());
                        crate::executor_kiro::execute_kiro(
                            &self.config.upstream_client,
                            &access_token,
                            region,
                            profile_arn,
                            &custom_req,
                            req.client_disconnected.clone(),
                            None,
                        )
                        .await
                    }
                    "antigravity" => {
                        let project_id = antigravity_project.as_deref().unwrap_or("");
                        crate::executor_antigravity::execute_antigravity(
                            &self.config.upstream_client,
                            &access_token,
                            project_id,
                            &custom_req,
                            req.client_disconnected.clone(),
                            req.stream_sink.as_ref(),
                            None,
                        )
                        .await
                    }
                    _ => unreachable!(),
                };

                return match executor_result {
                    Ok(response) => {
                        let total_ms = started.elapsed().as_millis() as u64;
                        let usage_row_id = match self.record_attempt_raw_with_tokens(
                            req,
                            combo,
                            target,
                            Some(&model),
                            None,
                            None,
                            None,
                            total_ms,
                            200,
                            attempt,
                            race_size,
                            trace_id,
                            response.usage.as_ref().map(|u| u.prompt_tokens),
                            response.usage.as_ref().map(|u| u.completion_tokens),
                            None,
                            None,
                            None,
                            None,
                            false,
                            true,
                            None,
                        ) {
                            Ok(id) => id,
                            Err(e) => {
                                tracing::warn!(error = %e, "record_attempt_raw_with_tokens failed; non-fatal");
                                None
                            }
                        };
                        PipelineResult {
                            status_code: 200,
                            error: None,
                            final_response: Some(response),
                            attempts: attempt,
                            usage_row_id,
                        }
                    }
                    Err(e) => {
                        if let CoreError::UpstreamError { status: 401, .. } = &e {
                            let provider_id_str = target.provider_id.to_string();
                            let dedup_key = format!(
                                "{}:{}",
                                crate::notifications::CODE_OAUTH_EXPIRED,
                                account_id.0
                            );
                            let payload = serde_json::json!({
                                "code": crate::notifications::CODE_OAUTH_EXPIRED,
                                "message": format!(
                                    "OAuth token for account {} on {} rejected by upstream (HTTP 401)",
                                    account_id.0, provider_id_str,
                                ),
                                "provider_id": &provider_id_str,
                                "details": {
                                    "account_id": account_id.0,
                                    "provider_id": &provider_id_str,
                                    "reason": "upstream_401",
                                },
                            });
                            let conn = self.conn.lock();
                            let _ = crate::notifications::insert_and_broadcast(
                                &conn,
                                crate::notifications::KIND_SYSTEM,
                                &payload,
                                Some(&dedup_key),
                                Some(&provider_id_str),
                            );
                        }
                        self.record_and_fail(
                            req,
                            combo,
                            target,
                            FailureContext {
                                attempt,
                                race_size,
                                err: &e,
                                started,
                                model: Some(&model),
                                connect_ms: None,
                                ttft_ms: None,
                                status_code: e.http_status(),
                            },
                        )
                    }
                };
            }
        }

        let adapter = match self.adapter_for(&target.provider_id) {
            Some(a) => a,
            None => {
                let err = CoreError::ProviderNotFound(target.provider_id.to_string());
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    FailureContext {
                        attempt,
                        race_size,
                        err: &err,
                        started,
                        model: None,
                        connect_ms: None,
                        ttft_ms: None,
                        status_code: 0,
                    },
                );
            }
        };

        let resolved_timeouts = {
            let model_overrides =
                match ModelTimeoutOverrides::from_json(model.timeout_overrides_json.as_deref()) {
                    Ok(o) => o,
                    Err(e) => {
                        return self.record_and_fail(
                            req,
                            combo,
                            target,
                            FailureContext {
                                attempt,
                                race_size,
                                err: &e,
                                started,
                                model: Some(&model),
                                connect_ms: None,
                                ttft_ms: None,
                                status_code: 0,
                            },
                        );
                    }
                };
            timeouts::resolve(&self.config.defaults, Some(&model_overrides))
        };
        tracing::debug!(
            combo_id = combo.id.0,
            target_id = target.id.0,
            provider = %target.provider_id,
            model = %model.model_id.as_str(),
            connect_ms = resolved_timeouts.connect.as_millis() as u64,
            request_send_ms = resolved_timeouts.request_send.as_millis() as u64,
            ttft_ms = resolved_timeouts.ttft.as_millis() as u64,
            idle_chunk_ms = resolved_timeouts.idle_chunk.as_millis() as u64,
            total_ms = resolved_timeouts.total.as_millis() as u64,
            "resolved timeouts for target"
        );

        let target_format = match adapter.format() {
            AdapterFormat::Openai => crate::models::TargetFormat::Openai,
            AdapterFormat::Anthropic => crate::models::TargetFormat::Anthropic,
            AdapterFormat::Mixed => model.target_format,
            AdapterFormat::Gemini => crate::models::TargetFormat::Gemini,
        };

        let mut upstream_req = req.openai_request.clone();
        upstream_req.model = model.model_id.as_str().to_string();
        if !req.openai_request.stream && req.stream_sink.is_some() {
            upstream_req.stream = true;
        }

        let compression_stats = if self.config.compression_mode != CompressionMode::Off {
            crate::compression::apply_compression(
                &mut upstream_req.messages,
                self.config.compression_mode,
            )
        } else {
            CompressionStats::empty()
        };
        *self.compression_stats_cell.write() = Some(compression_stats);

        let body_bytes: bytes::Bytes = match target_format {
            crate::models::TargetFormat::Openai => {
                match self.normalize_and_serialize(
                    &upstream_req,
                    adapter.as_ref(),
                    "openai request",
                    req,
                    combo,
                    target,
                    attempt,
                    race_size,
                    started,
                    &model,
                ) {
                    Ok(b) => b,
                    Err(r) => return r,
                }
            }
            crate::models::TargetFormat::Anthropic => {
                let anthro = crate::translation::openai_to_anthropic(&upstream_req);
                match self.normalize_and_serialize(
                    &anthro,
                    adapter.as_ref(),
                    "anthropic request",
                    req,
                    combo,
                    target,
                    attempt,
                    race_size,
                    started,
                    &model,
                ) {
                    Ok(b) => b,
                    Err(r) => return r,
                }
            }
            crate::models::TargetFormat::Gemini => {
                let gemini = crate::translation::openai_to_gemini(&upstream_req);
                match self.normalize_and_serialize(
                    &gemini,
                    adapter.as_ref(),
                    "gemini request",
                    req,
                    combo,
                    target,
                    attempt,
                    race_size,
                    started,
                    &model,
                ) {
                    Ok(b) => b,
                    Err(r) => return r,
                }
            }
        };

        let (api_key, account_label) = match self.resolve_target_api_key_and_label(target) {
            Ok(v) => v,
            Err(e) => {
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    FailureContext {
                        attempt,
                        race_size,
                        err: &e,
                        started,
                        model: Some(&model),
                        connect_ms: None,
                        ttft_ms: None,
                        status_code: 0,
                    },
                );
            }
        };

        let account_label_str = account_label.as_deref().unwrap_or("");
        let url =
            adapter.build_chat_url_for_account(target_format, &model.model_id, account_label_str);
        let headers = adapter.build_headers(&api_key, target_format, &model.model_id);

        if race_cancel.is_cancelled() {
            return self.record_and_fail_with_trace_id(
                req,
                combo,
                target,
                FailureContext {
                    attempt,
                    race_size,
                    err: &CoreError::RaceLost,
                    started,
                    model: Some(&model),
                    connect_ms: None,
                    ttft_ms: None,
                    status_code: CoreError::RaceLost.http_status(),
                },
                trace_id,
            );
        }

        let compression_stats_at_connecting = self.compression_stats_cell.read().clone();
        crate::usage::publish_stage_event(crate::usage::StageEvent {
            request_id: req.request_id.to_string(),
            trace_id: trace_id.to_string(),
            provider_id: target.provider_id.to_string(),
            upstream_model_id: model.model_id.as_str().to_string(),
            stage: "connecting".into(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            connect_ms: None,
            ttft_ms: None,
            status_code: 0,
            error: None,
            stop_reason: None,
            compression_savings_pct: compression_stats_at_connecting
                .as_ref()
                .and_then(|s| s.savings_pct_opt()),
            compression_techniques: compression_stats_at_connecting
                .as_ref()
                .and_then(|s| s.techniques_csv()),
            timestamp: String::new(),
            endpoint_kind: crate::endpoint::EndpointKind::Chat,
        });

        let result = self
            .dispatch_upstream(
                target,
                combo,
                req,
                &model,
                target_format,
                &url,
                &headers,
                body_bytes,
                &resolved_timeouts,
                started,
                attempt,
                race_size,
                trace_id,
            )
            .await;

        if let Some(aid) = target.account_id {
            match &result.error {
                Some(CoreError::ClientDisconnected) => {
                    tracing::debug!(
                        account_id = aid.0,
                        "client cancelled; leaving circuit breaker untouched"
                    );
                }
                Some(e) if RetryPolicy::is_retryable(e, self.config.idle_chunk_retryable) => {
                    let outcome = self.circuit_breaker.record_failure_outcome(aid);
                    if outcome.just_opened {
                        let provider_id_str = target.provider_id.to_string();
                        let model_id_str = model.model_id.as_str().to_string();
                        let combo_target_id = target.id.0;
                        let conn = self.conn.lock();
                        let dedup_key =
                            format!("{}:{}", crate::notifications::CODE_CIRCUIT_OPEN, aid.0);
                        let payload = serde_json::json!({
                            "code": crate::notifications::CODE_CIRCUIT_OPEN,
                            "message": format!(
                                "Circuit breaker opened for account {} on {} ({}) — {}/{} failures",
                                aid.0, provider_id_str, model_id_str,
                                outcome.consecutive_failures, outcome.threshold,
                            ),
                            "provider_id": &provider_id_str,
                            "details": {
                                "combo_target_id": combo_target_id,
                                "account_id": aid.0,
                                "provider_id": &provider_id_str,
                                "model_id": &model_id_str,
                                "failure_count": outcome.consecutive_failures,
                                "threshold": outcome.threshold,
                            },
                        });
                        let _ = crate::notifications::insert_and_broadcast(
                            &conn,
                            crate::notifications::KIND_SYSTEM,
                            &payload,
                            Some(&dedup_key),
                            Some(&provider_id_str),
                        );
                    }
                }
                _ => {
                    self.circuit_breaker.record_success(aid);
                }
            }
        }

        result
    }

    pub(crate) fn load_combo(&self, req: &PipelineRequest) -> Result<Combo> {
        if let Some(combo) = req.combo_override.as_ref() {
            return Ok(combo.clone());
        }
        let conn = self.conn.lock();
        combos::get_combo(&conn, req.combo_id)?.ok_or(CoreError::ComboNotFound(req.combo_id.0))
    }

    pub(crate) fn resolve_targets(
        &self,
        combo: &Combo,
        targets_override: Option<&[ComboTarget]>,
    ) -> Result<Vec<ComboTarget>> {
        if let Some(overrides) = targets_override {
            let conn = self.conn.lock();
            return combos::expand_account_rotation(&conn, overrides.to_vec());
        }

        let conn = self.conn.lock();
        let _ = combos::list_targets(&conn, combo.id)?;
        let ordered = combos::resolve_target_order_with_mode(
            &conn,
            combo,
            &self.rr_counters,
            &self.selection_registry,
        )?;
        combos::expand_account_rotation(&conn, ordered)
    }

    fn adapter_for(
        &self,
        provider_id: &crate::ids::ProviderId,
    ) -> Option<Arc<dyn ProviderAdapter>> {
        self.config
            .adapters
            .iter()
            .find(|a| a.id() == provider_id)
            .cloned()
    }

    fn load_model(&self, row_id: crate::ids::ModelRowId) -> Result<Model> {
        let conn = self.conn.lock();
        models::get_by_row_id(&conn, row_id)?.ok_or(CoreError::ModelNotFound {
            provider: "<unknown>".into(),
            model: format!("row_id={}", row_id.0),
        })
    }

    #[cfg(test)]
    pub(crate) fn decrypt_account_key(&self, account_id: crate::ids::AccountId) -> Result<String> {
        let conn = self.conn.lock();
        crate::accounts::decrypt_api_key(&conn, account_id, &self.config.master_key)
    }

    #[cfg(test)]
    pub(crate) fn resolve_target_api_key(&self, target: &ComboTarget) -> Result<String> {
        match target.account_id {
            Some(account_id) => self.decrypt_account_key(account_id),
            None => {
                let conn = self.conn.lock();
                match crate::providers::get(&conn, &target.provider_id)? {
                    Some(p) if matches!(p.auth_type, crate::providers::AuthType::None) => {
                        Ok(String::new())
                    }
                    Some(p) if p.id.0 == "opencode-zen" => {
                        Ok(String::new())
                    }
                    _ => Err(CoreError::Auth(format!(
                        "combo_target {} has no account_id after expansion",
                        target.id.0
                    ))),
                }
            }
        }
    }

    fn resolve_target_api_key_and_label(
        &self,
        target: &ComboTarget,
    ) -> Result<(String, Option<String>)> {
        match target.account_id {
            Some(account_id) => {
                let conn = self.conn.lock();
                crate::accounts::decrypt_api_key_and_label(
                    &conn,
                    account_id,
                    &self.config.master_key,
                )
            }
            None => {
                let conn = self.conn.lock();
                match crate::providers::get(&conn, &target.provider_id)? {
                    Some(p) if matches!(p.auth_type, crate::providers::AuthType::None) => {
                        Ok((String::new(), None))
                    }
                    Some(p) if p.id.0 == "opencode-zen" => {
                        Ok((String::new(), None))
                    }
                    _ => Err(CoreError::Auth(format!(
                        "combo_target {} has no account_id after expansion",
                        target.id.0
                    ))),
                }
            }
        }
    }

    pub(crate) fn mark_client_response(&self, usage_row_id: Option<UsageId>) {
        let Some(id) = usage_row_id else { return };
        let conn = match self.conn.try_lock_for(crate::db::conn::HOT_PATH_LOCK_TIMEOUT) {
            Some(g) => g,
            None => {
                tracing::warn!(
                    usage_row_id = id.0,
                    "writer lock unavailable within 100ms; skipping client_response UPDATE"
                );
                return;
            }
        };
        match conn.execute(
            "UPDATE usage SET client_response = 1 WHERE id = ?1",
            params![id.0],
        ) {
            Ok(n) if n > 0 => {
                tracing::debug!(
                    usage_row_id = id.0,
                    "marked usage row as client_response = true"
                );
                if let Ok(Some(updated_row)) = crate::usage::row_for_broadcast_by_id(&conn, id.0) {
                    crate::usage::publish_usage_row(updated_row);
                }
            }
            Ok(_) => {
                tracing::warn!(
                    usage_row_id = id.0,
                    "client_response UPDATE matched 0 rows — row may have been dropped"
                );
            }
            Err(e) => {
                tracing::warn!(
                    usage_row_id = id.0,
                    error = %e,
                    "client_response UPDATE failed; non-fatal"
                );
            }
        }
    }

    pub(crate) fn failure(&self, err: CoreError, attempts: u8, _phase: ErrorPhase) -> PipelineResult {
        PipelineResult {
            status_code: err.http_status(),
            error: Some(err),
            final_response: None,
            attempts,
            usage_row_id: None,
        }
    }

    pub(crate) fn client_disconnected_result(&self, attempts: u8) -> PipelineResult {
        self.failure(CoreError::ClientDisconnected, attempts, ErrorPhase::Retry)
    }

    pub(crate) fn is_client_disconnected(&self, rx: &mut watch::Receiver<bool>) -> bool {
        *rx.borrow_and_update()
    }

    pub(crate) fn check_and_trigger_proxy_rotation(
        &self,
        provider_id: &crate::ids::ProviderId,
        status_code: Option<u16>,
        is_connect_error: bool,
    ) {
        let conn = self.conn.lock();
        if let Ok(Some(provider)) = crate::providers::get(&conn, provider_id) {
            if provider.use_proxies {
                let mut should_rotate = false;
                let errors_list: Vec<&str> = provider.proxy_rotation_errors
                    .split(',')
                    .map(|s| s.trim())
                    .collect();

                if let Some(sc) = status_code {
                    let sc_str = sc.to_string();
                    if errors_list.contains(&sc_str.as_str()) {
                        should_rotate = true;
                    }
                }

                if is_connect_error {
                    if errors_list.contains(&"connect_error") || errors_list.contains(&"timeout") {
                        should_rotate = true;
                    }
                }

                if should_rotate {
                    if let Some(ref bad_proxy_id) = provider.current_proxy_id {
                        tracing::warn!(
                            provider = %provider_id,
                            proxy_id = %bad_proxy_id,
                            "proxy rotation triggered: marking proxy as dead and clearing binding"
                        );
                        let _ = crate::free_proxies::update_proxy_status(&conn, bad_proxy_id, "dead", None);
                        let _ = crate::providers::update_current_proxy(&conn, provider_id, None);
                    }
                }
            }
        }
    }

    pub(crate) fn record_and_fail(
        &self,
        req: &PipelineRequest,
        combo: &Combo,
        target: &ComboTarget,
        ctx: FailureContext<'_>,
    ) -> PipelineResult {
        self.record_and_fail_with_trace_id(req, combo, target, ctx, req.trace_id.to_string())
    }

    pub(crate) fn record_and_fail_with_trace_id(
        &self,
        req: &PipelineRequest,
        combo: &Combo,
        target: &ComboTarget,
        ctx: FailureContext<'_>,
        trace_id: String,
    ) -> PipelineResult {
        self.record_and_fail_with_trace_id_and_partial(
            req, combo, target, ctx, trace_id, None, None, 0, "",
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_and_fail_with_trace_id_and_partial(
        &self,
        req: &PipelineRequest,
        combo: &Combo,
        target: &ComboTarget,
        ctx: FailureContext<'_>,
        trace_id: String,
        acc: Option<&crate::sse_accumulator::ResponseAccumulator>,
        chunk_id: Option<&str>,
        created: u64,
        model_name: &str,
    ) -> PipelineResult {
        let FailureContext {
            attempt,
            race_size,
            err,
            started,
            model,
            connect_ms,
            ttft_ms,
            status_code,
        } = ctx;
        let total_ms = started.elapsed().as_millis() as u64;
        let request_body_json = req
            .request_body_json
            .clone()
            .or_else(|| serde_json::to_value(&req.openai_request).ok());
        let request_headers = crate::redact::redact_btreemap_sensitive(req.request_headers.clone());
        let response_body_json: Option<serde_json::Value> =
            acc.filter(|a| !a.is_completely_empty()).map(|a| {
                let chunk_id_str = chunk_id.unwrap_or("partial");
                a.finish(chunk_id_str, created, model_name)
            });
        let was_streaming = req.stream_sink.is_some();
        let (is_streaming, stream_complete) = if response_body_json.is_some() {
            (true, false)
        } else {
            (was_streaming, false)
        };
        let usage_row_id = match self.record_attempt_raw_with_tokens(
            req,
            combo,
            target,
            model,
            Some(err),
            connect_ms,
            ttft_ms,
            total_ms,
            status_code,
            attempt,
            race_size,
            trace_id,
            None,
            None,
            request_body_json,
            response_body_json,
            Some(request_headers),
            None,
            is_streaming,
            stream_complete,
            None,
        ) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(error = %e, "record_attempt_raw_with_tokens failed; non-fatal");
                None
            }
        };
        PipelineResult {
            status_code: err.http_status(),
            error: Some(err.clone_for_result()),
            final_response: None,
            attempts: attempt,
            usage_row_id,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn fail_stream_client_disconnected(
        &self,
        req: &PipelineRequest,
        combo: &Combo,
        target: &ComboTarget,
        attempt: u8,
        race_size: u8,
        started: Instant,
        model: &Model,
        connect_ms: u64,
        ttft_ms: Option<u64>,
        trace_id: String,
        acc: Option<&mut crate::sse_accumulator::ResponseAccumulator>,
        chunk_id: &str,
        created: u64,
        model_name: &str,
    ) -> PipelineResult {
        let has_partial_content = acc
            .as_ref()
            .is_some_and(|a| !a.is_empty());
        if let Some(ref a) = acc {
            if let Some((code, message)) = a.extract_upstream_error_from_raw() {
                tracing::warn!(
                    combo_id = combo.id.0,
                    target_id = target.id.0,
                    provider = %target.provider_id,
                    model = %model.model_id.as_str(),
                    inline_error_code = code,
                    inline_error_message = %message,
                    "client disconnected but upstream had sent inline SSE error \
                     (code={}); attributing to upstream error, not client disconnect",
                    code,
                );
                let err = CoreError::UpstreamError {
                    status: code,
                    provider: target.provider_id.to_string(),
                    model: model_name.to_string(),
                    body: message,
                };
                let acc_ref: Option<&crate::sse_accumulator::ResponseAccumulator> = match acc {
                    Some(a) => {
                        a.mark_partial();
                        Some(&*a)
                    }
                    None => None,
                };
                return self.record_and_fail_with_trace_id_and_partial(
                    req,
                    combo,
                    target,
                    FailureContext {
                        attempt,
                        race_size,
                        err: &err,
                        started,
                        model: Some(model),
                        connect_ms: Some(connect_ms),
                        ttft_ms,
                        status_code: code,
                    },
                    trace_id,
                    acc_ref,
                    Some(chunk_id),
                    created,
                    model_name,
                );
            }
        }
        let acc_ref: Option<&crate::sse_accumulator::ResponseAccumulator> = match acc {
            Some(a) => {
                a.mark_partial();
                Some(&*a)
            }
            None => None,
        };
        let err: CoreError = if has_partial_content {
            CoreError::UpstreamConnection(
                "stream interrupted — client disconnected after receiving partial content".into()
            )
        } else {
            CoreError::ClientDisconnected
        };
        self.record_and_fail_with_trace_id_and_partial(
            req,
            combo,
            target,
            FailureContext {
                attempt,
                race_size,
                err: &err,
                started,
                model: Some(model),
                connect_ms: Some(connect_ms),
                ttft_ms,
                status_code: 499,
            },
            trace_id,
            acc_ref,
            Some(chunk_id),
            created,
            model_name,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn fail_on_sink_send_error(
        &self,
        e: crate::race_sink::StreamSinkError,
        req: &PipelineRequest,
        combo: &Combo,
        target: &ComboTarget,
        attempt: u8,
        race_size: u8,
        started: Instant,
        model: &Model,
        connect_ms: u64,
        ttft_ms: Option<u64>,
        trace_id: String,
        acc: Option<&mut crate::sse_accumulator::ResponseAccumulator>,
        chunk_id: &str,
        created: u64,
        model_name: &str,
    ) -> PipelineResult {
        let err = match e {
            crate::race_sink::StreamSinkError::Lost => {
                tracing::debug!(
                    combo_id = combo.id.0,
                    target_id = target.id.0,
                    "sink send failed: Lost (another race lane won)"
                );
                CoreError::RaceLost
            }
            crate::race_sink::StreamSinkError::Closed => {
                let elapsed = started.elapsed().as_millis() as u64;
                let watchdog_fired = *req.client_disconnected.borrow();
                if let Some(ref a) = acc {
                    if let Some((code, message)) = a.extract_upstream_error_from_raw() {
                        tracing::warn!(
                            combo_id = combo.id.0,
                            target_id = target.id.0,
                            provider = %target.provider_id,
                            model = %model.model_id.as_str(),
                            elapsed_ms = elapsed,
                            inline_error_code = code,
                            inline_error_message = %message,
                            "sink closed after upstream sent inline SSE error \
                             (code={}, elapsed={}ms); attributing to upstream, \
                             not client disconnect",
                            code, elapsed
                        );
                        return {
                            let err = CoreError::UpstreamError {
                                status: code,
                                provider: target.provider_id.to_string(),
                                model: model_name.to_string(),
                                body: message,
                            };
                            let acc_ref: Option<&crate::sse_accumulator::ResponseAccumulator> =
                                match acc {
                                    Some(a) => {
                                        a.mark_partial();
                                        Some(&*a)
                                    }
                                    None => None,
                                };
                            self.record_and_fail_with_trace_id_and_partial(
                                req,
                                combo,
                                target,
                                FailureContext {
                                    attempt,
                                    race_size,
                                    err: &err,
                                    started,
                                    model: Some(model),
                                    connect_ms: Some(connect_ms),
                                    ttft_ms: None,
                                    status_code: code,
                                },
                                trace_id,
                                acc_ref,
                                Some(chunk_id),
                                created,
                                model_name,
                            )
                        };
                    }
                }
                tracing::warn!(
                    combo_id = combo.id.0,
                    target_id = target.id.0,
                    provider = %target.provider_id,
                    model = %model.model_id.as_str(),
                    elapsed_ms = elapsed,
                    connect_ms = connect_ms,
                    ttft_ms = ?ttft_ms,
                    watchdog_fired,
                    "sink send failed: Closed — client/proxy disconnected \
                     (elapsed={}ms, connect={}ms, ttft={:?}, watchdog_fired={})",
                    elapsed, connect_ms, ttft_ms, watchdog_fired
                );
                CoreError::UpstreamConnection(format!(
                    "client disconnected (elapsed={}ms, connect={}ms, ttft={:?}) — \
                     likely proxy idle timeout or client HTTP library timeout",
                    elapsed, connect_ms, ttft_ms
                ))
            }
        };
        let acc_ref: Option<&crate::sse_accumulator::ResponseAccumulator> = match acc {
            Some(a) => {
                a.mark_partial();
                Some(&*a)
            }
            None => None,
        };
        self.record_and_fail_with_trace_id_and_partial(
            req,
            combo,
            target,
            FailureContext {
                attempt,
                race_size,
                err: &err,
                started,
                model: Some(model),
                connect_ms: Some(connect_ms),
                ttft_ms,
                status_code: err.http_status(),
            },
            trace_id,
            acc_ref,
            Some(chunk_id),
            created,
            model_name,
        )
    }

    pub(crate) fn record_attempt_raw_with_tokens(
        &self,
        req: &PipelineRequest,
        combo: &Combo,
        target: &ComboTarget,
        model: Option<&Model>,
        err: Option<&CoreError>,
        connect_ms: Option<u64>,
        ttft_ms: Option<u64>,
        total_ms: u64,
        status_code: u16,
        attempt: u8,
        race_size: u8,
        trace_id: String,
        prompt_tokens: Option<u32>,
        completion_tokens: Option<u32>,
        request_body_json: Option<serde_json::Value>,
        response_body_json: Option<serde_json::Value>,
        request_headers: Option<std::collections::BTreeMap<String, String>>,
        response_headers: Option<std::collections::BTreeMap<String, String>>,
        is_streaming: bool,
        stream_complete: bool,
        stop_reason: Option<String>,
    ) -> Result<Option<UsageId>> {
        let recording = self.is_recording();
        let compression_stats_snapshot = self.compression_stats_cell.read().clone();
        let compression_savings_pct = compression_stats_snapshot
            .as_ref()
            .and_then(|s| s.savings_pct_opt());
        let compression_techniques = compression_stats_snapshot
            .as_ref()
            .and_then(|s| s.techniques_csv());

        let (prompt_tokens, prompt_tokens_estimated) = match prompt_tokens {
            Some(t) if t > 0 => (Some(t), false),
            _ => {
                let est = crate::token_estimate::estimate_prompt_tokens(&req.openai_request.messages);
                if est > 0 {
                    tracing::debug!(
                        request_id = %req.request_id,
                        estimated_prompt_tokens = est,
                        "upstream did not report usage; estimated prompt tokens from request messages"
                    );
                    (Some(est), true)
                } else {
                    (None, false)
                }
            }
        };

        let (completion_tokens, completion_tokens_estimated) = match completion_tokens {
            Some(t) if t > 0 => (Some(t), false),
            _ => {
                let completion_text = response_body_json
                    .as_ref()
                    .and_then(|v| {
                        v.get("choices")
                            .and_then(|c| c.get(0))
                            .and_then(|c| c.get("message"))
                            .and_then(|m| m.get("content"))
                            .and_then(|c| c.as_str())
                    })
                    .unwrap_or("");
                if !completion_text.is_empty() {
                    let est = crate::token_estimate::estimate_completion_tokens(completion_text);
                    tracing::debug!(
                        request_id = %req.request_id,
                        estimated_completion_tokens = est,
                        "upstream did not report usage; estimated completion tokens from response body"
                    );
                    (Some(est), true)
                } else {
                    (None, false)
                }
            }
        };

        let input = UsageInput {
            request_id: req.request_id,
            trace_id: trace_id.clone(),
            attempt,
            provider_id: target.provider_id.clone(),
            account_id: target.account_id,
            combo_id: Some(combo.id),
            combo_target_id: Some(target.id),
            model_row_id: model.map(|m| m.row_id),
            upstream_model_id: model
                .map(|m| m.model_id.as_str().to_string())
                .unwrap_or_default(),
            prompt_tokens,
            completion_tokens,
            connect_ms,
            ttft_ms,
            total_ms,
            status_code,
            error_msg: err.map(|e| format!("{}", e)),
            race_total: race_size,
            race_lost: err.is_some() && req.race_cancelled,
            api_key_id: req.api_key_id,
            request_body_json: if recording { request_body_json } else { None },
            response_body_json: if recording { response_body_json } else { None },
            request_headers: if recording { request_headers } else { None },
            response_headers: if recording { response_headers } else { None },
            error_message: err.map(|e| format!("{}", e)),
            race_attempts: race_size,
            is_streaming,
            stream_complete,
            stop_reason: stop_reason.clone(),
            compression_savings_pct,
            compression_techniques,
            client_response: false,
            prompt_tokens_estimated,
            completion_tokens_estimated,
            endpoint_kind: crate::endpoint::EndpointKind::Chat,
        };

        {
            let stage_label: &str = if err.is_none() {
                "completed"
            } else if req.race_cancelled {
                "cancelled"
            } else {
                "failed"
            };
            let error_str: Option<String> =
                err.map(|e| crate::cost::redact_error_msg(&e.to_string()).0);
            let terminal_snapshot = self.compression_stats_cell.read().clone();
            crate::usage::publish_stage_event(crate::usage::StageEvent {
                request_id: req.request_id.to_string(),
                trace_id: trace_id.to_string(),
                provider_id: target.provider_id.to_string(),
                upstream_model_id: model
                    .map(|m| m.model_id.as_str().to_string())
                    .unwrap_or_default(),
                stage: stage_label.into(),
                elapsed_ms: total_ms,
                connect_ms,
                ttft_ms,
                status_code,
                error: error_str,
                stop_reason: stop_reason.clone(),
                compression_savings_pct: terminal_snapshot
                    .as_ref()
                    .and_then(|s| s.savings_pct_opt()),
                compression_techniques: terminal_snapshot.as_ref().and_then(|s| s.techniques_csv()),
                timestamp: String::new(),
                endpoint_kind: crate::endpoint::EndpointKind::Chat,
            });
        }

        let conn = match self
            .conn
            .try_lock_for(crate::db::conn::HOT_PATH_LOCK_TIMEOUT)
        {
            Some(g) => g,
            None => {
                tracing::warn!(
                    request_id = %req.request_id,
                    "writer lock unavailable within 100ms; dropping usage row"
                );
                return Ok(None);
            }
        };
        let usage_id = cost::record(&conn, &input)?;
        Ok(Some(usage_id))
    }
}
