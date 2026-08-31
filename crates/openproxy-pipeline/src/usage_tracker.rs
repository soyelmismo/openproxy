use parking_lot::{Mutex, RwLock};
use rusqlite::Connection;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{FailureContext, PipelineRequest, PipelineResult, is_upstream_health_issue};
use openproxy_compression::stats::CompressionStats;
use openproxy_types::SelectionRegistry;
use openproxy_types::combos::{Combo, ComboTarget};
use openproxy_types::error::{CoreError, Result};
use openproxy_types::models::Model;
use openproxy_types::usage::{
    USAGE_FLAG_CLIENT_RESPONSE, USAGE_FLAG_COMPLETION_ESTIMATED, USAGE_FLAG_IS_STREAMING,
    USAGE_FLAG_PROMPT_ESTIMATED, USAGE_FLAG_PROXY_ROTATED, USAGE_FLAG_RACE_LOST,
    USAGE_FLAG_STREAM_COMPLETE, UsageInput,
};

#[derive(Clone)]
pub struct UsageTracker {
    pub conn: Arc<Mutex<Connection>>,
    pub background_tx: tokio::sync::mpsc::Sender<crate::worker::BackgroundJob>,
    pub record_bodies_and_headers: Arc<AtomicBool>,
    pub compression_stats_cell: Arc<RwLock<Option<CompressionStats>>>,
    pub selection_registry: Arc<SelectionRegistry>,
    pub cooldown_secs: u64,
    pub cooldown_max_secs: u64,
    pub cooldown_factor: u32,
    pub repo: Arc<dyn crate::repository::PipelineRepository>,
}

pub trait UsageTrackerTrait: Send + Sync {
    fn is_recording(&self) -> bool;
    fn set_recording(&self, enabled: bool);
}

impl UsageTrackerTrait for UsageTracker {
    fn is_recording(&self) -> bool {
        self.is_recording()
    }

    fn set_recording(&self, enabled: bool) {
        self.set_recording(enabled);
    }
}

impl UsageTracker {
    pub fn is_recording(&self) -> bool {
        self.record_bodies_and_headers.load(Ordering::Relaxed)
    }

    pub fn set_recording(&self, enabled: bool) {
        self.record_bodies_and_headers
            .store(enabled, Ordering::Relaxed);
    }

    pub(crate) fn mark_client_response(
        &self,
        usage_tuple: Option<(
            openproxy_types::ids::RequestId,
            u8,
            openproxy_types::ids::ComboTargetId,
        )>,
    ) {
        let Some((request_id, attempt, target_id)) = usage_tuple else {
            return;
        };
        let job = crate::worker::BackgroundJob::MarkClientResponse {
            request_id: request_id.to_string(),
            attempt,
            target_id,
        };
        if let Err(e) = self.background_tx.try_send(job) {
            if matches!(e, tokio::sync::mpsc::error::TrySendError::Closed(_)) {
                let job = e.into_inner();
                let conn = Arc::clone(&self.conn);
                let repo = Arc::clone(&self.repo);
                let selection_registry = Arc::clone(&self.selection_registry);
                drop(tokio::task::spawn_blocking(move || {
                    crate::worker::process_job(&conn, repo.as_ref(), job, &selection_registry);
                }));
            } else {
                tracing::warn!(
                    "failed to send MarkClientResponse to background worker: {}",
                    e
                );
            }
        }
    }

    pub(crate) fn record_no_healthy_targets_row(
        &self,
        req: &PipelineRequest,
        combo: &Combo,
        started: std::time::Instant,
    ) {
        let input = UsageInput {
            proxy_url: None,
            proxy_status: None,
            request_id: req.request_id,
            trace_id: req.trace_id.to_string(),
            attempt: 1,
            provider_id: openproxy_types::ids::ProviderId::new(""),
            account_id: None,
            combo_id: Some(combo.id),
            combo_target_id: None,
            model_row_id: None,
            upstream_model_id: req.openai_request.model.clone(),
            prompt_tokens: None,
            completion_tokens: None,
            cached_tokens: None,
            connect_ms: None,
            ttft_ms: None,
            total_ms: started.elapsed().as_millis() as u64,
            status_code: 502,
            error_msg: Some("no_healthy_targets".to_string()),
            race_total: 1,
            api_key_id: req.api_key_id,
            compression_savings_pct: None,
            compression_techniques: None,
            request_body_json: None,
            response_body_json: None,
            request_headers: None,
            response_headers: None,
            error_message: Some("no_healthy_targets".to_string()),
            race_attempts: 1,
            stop_reason: None,
            flags: USAGE_FLAG_CLIENT_RESPONSE,
            endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
        };
        let conn = Arc::clone(&self.conn);
        drop(tokio::task::spawn_blocking(move || {
            let lock = conn.lock();
            let _ = openproxy_db::cost::record(&lock, &input);
        }));
    }

    pub(crate) fn record_predictive_skipped_row(
        &self,
        req: &PipelineRequest,
        combo: &Combo,
        target: &crate::context::ResolvedTarget,
        attempt: u8,
    ) {
        use std::fmt::Write;
        let mut trace_id = String::with_capacity(40);
        let _ = write!(&mut trace_id, "{}:{}", req.trace_id, attempt);
        let input = UsageInput {
            proxy_url: None,
            proxy_status: None,
            request_id: req.request_id,
            trace_id,
            attempt,
            provider_id: target.target.provider_id.clone(),
            account_id: target.target.account_id,
            combo_id: Some(combo.id),
            combo_target_id: Some(target.target.id),
            model_row_id: target.target.model_row_id,
            upstream_model_id: target.model.model_id.0.clone(),
            prompt_tokens: None,
            completion_tokens: None,
            cached_tokens: None,
            connect_ms: None,
            ttft_ms: None,
            total_ms: 0,
            status_code: 0,
            error_msg: Some("predict_skipped".to_string()),
            race_total: 1,
            api_key_id: req.api_key_id,
            compression_savings_pct: None,
            compression_techniques: None,
            request_body_json: None,
            response_body_json: None,
            request_headers: None,
            response_headers: None,
            error_message: Some("predictive rate limit: skipped to avoid 429".to_string()),
            race_attempts: 1,
            stop_reason: None,
            flags: 0,
            endpoint_kind: req.endpoint_kind,
        };
        let conn = Arc::clone(&self.conn);
        drop(tokio::task::spawn_blocking(move || {
            let lock = conn.lock();
            let _ = openproxy_db::cost::record(&lock, &input);
        }));
    }

    pub(crate) fn record_and_fail_with_trace_id_and_partial(
        &self,
        params: crate::PartialFailureParams<'_>,
    ) -> PipelineResult {
        let crate::PartialFailureParams {
            req,
            combo,
            target,
            ctx,
            trace_id,
            acc,
            chunk_id,
            created,
            model_name,
        } = params;
        let FailureContext {
            attempt,
            race_size,
            err,
            started,
            model,
            connect_ms,
            ttft_ms,
            status_code,
            proxy_url,
            proxy_status,
        } = ctx;
        let total_ms = started.elapsed().as_millis() as u64;
        let request_headers = if self.is_recording() {
            Some(crate::redact::redact_btreemap_sensitive(
                req.request_headers.clone(),
            ))
        } else {
            None
        };
        let response_body_json: Option<serde_json::Value> =
            acc.filter(|a| !a.is_completely_empty()).map(|a| {
                let chunk_id_str = chunk_id.unwrap_or("partial");
                a.finish(chunk_id_str, created, model_name)
            });

        let is_streaming = req.stream_sink.is_some() || req.openai_request.stream;
        let stream_complete = false;

        let usage_tuple = match UsageRecordBuilder::new(self, req, combo, target)
            .model_opt(model)
            .err(err)
            .connect_ms_opt(connect_ms)
            .ttft_ms_opt(ttft_ms)
            .total_ms(total_ms)
            .status_code(status_code)
            .attempt(attempt)
            .race_size(race_size)
            .trace_id(trace_id)
            .proxy_url(proxy_url)
            .proxy_status(proxy_status)
            .is_proxy_rotated(err.is_proxy_rotated())
            .response_body_json(response_body_json)
            .request_headers(request_headers)
            .is_streaming(is_streaming)
            .stream_complete(stream_complete)
            .record()
        {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(error = %e, "UsageRecordBuilder failed; non-fatal");
                None
            }
        };

        PipelineResult {
            status_code: err.http_status(),
            error: Some(err.clone_for_result()),
            final_response: None,
            attempts: attempt,
            usage_tuple,
        }
    }
}

pub struct UsageRecordBuilder<'a> {
    pub(crate) tracker: &'a UsageTracker,
    pub(crate) req: PipelineRequest,
    pub(crate) combo: &'a Combo,
    pub(crate) target: &'a ComboTarget,
    pub(crate) model: Option<&'a Model>,
    pub(crate) err: Option<&'a CoreError>,
    pub(crate) connect_ms: Option<u64>,
    pub(crate) ttft_ms: Option<u64>,
    pub(crate) total_ms: u64,
    pub(crate) status_code: u16,
    pub(crate) attempt: u8,
    pub(crate) race_size: u8,
    pub(crate) total_targets: u8,
    pub(crate) trace_id: String,
    pub(crate) prompt_tokens: Option<u32>,
    pub(crate) completion_tokens: Option<u32>,
    pub(crate) cached_tokens: Option<u32>,
    pub(crate) response_body_json: Option<serde_json::Value>,
    pub(crate) request_headers: Option<std::collections::BTreeMap<String, String>>,
    pub(crate) response_headers: Option<std::collections::BTreeMap<String, String>>,
    pub(crate) is_streaming: bool,
    pub(crate) stream_complete: bool,
    pub(crate) stop_reason: Option<String>,
    pub(crate) proxy_url: Option<String>,
    pub(crate) proxy_status: Option<String>,
    pub(crate) is_proxy_rotated: bool,
}

impl<'a> UsageRecordBuilder<'a> {
    pub fn new(
        tracker: &'a UsageTracker,
        req: PipelineRequest,
        combo: &'a Combo,
        target: &'a ComboTarget,
    ) -> Self {
        let trace_id = req.trace_id.to_string();
        Self {
            tracker,
            req,
            combo,
            target,
            model: None,
            err: None,
            connect_ms: None,
            ttft_ms: None,
            total_ms: 0,
            status_code: 0,
            attempt: 1,
            race_size: 1,
            total_targets: 1,
            trace_id,
            prompt_tokens: None,
            completion_tokens: None,
            cached_tokens: None,
            response_body_json: None,
            request_headers: None,
            response_headers: None,
            is_streaming: false,
            stream_complete: false,
            stop_reason: None,
            proxy_url: None,
            proxy_status: None,
            is_proxy_rotated: false,
        }
    }

    pub fn model_opt(mut self, model: Option<&'a Model>) -> Self {
        self.model = model;
        self
    }
    pub fn err(mut self, err: &'a CoreError) -> Self {
        self.err = Some(err);
        self
    }
    pub fn err_opt(mut self, err: Option<&'a CoreError>) -> Self {
        self.err = err;
        self
    }
    pub fn connect_ms_opt(mut self, connect_ms: Option<u64>) -> Self {
        self.connect_ms = connect_ms;
        self
    }
    pub fn ttft_ms_opt(mut self, ttft_ms: Option<u64>) -> Self {
        self.ttft_ms = ttft_ms;
        self
    }
    pub fn total_ms(mut self, total_ms: u64) -> Self {
        self.total_ms = total_ms;
        self
    }
    pub fn status_code(mut self, status_code: u16) -> Self {
        self.status_code = status_code;
        self
    }
    pub fn attempt(mut self, attempt: u8) -> Self {
        self.attempt = attempt;
        self
    }
    pub fn race_size(mut self, race_size: u8) -> Self {
        self.race_size = race_size;
        self
    }

    pub fn total_targets(mut self, total_targets: u8) -> Self {
        self.total_targets = total_targets;
        self
    }
    pub fn trace_id(mut self, trace_id: String) -> Self {
        self.trace_id = trace_id;
        self
    }
    pub fn prompt_tokens_opt(mut self, prompt_tokens: Option<u32>) -> Self {
        self.prompt_tokens = prompt_tokens;
        self
    }

    pub fn cached_tokens(mut self, cached_tokens: Option<u32>) -> Self {
        self.cached_tokens = cached_tokens;
        self
    }
    pub fn completion_tokens_opt(mut self, completion_tokens: Option<u32>) -> Self {
        self.completion_tokens = completion_tokens;
        self
    }
    pub fn response_body_json(mut self, response_body_json: Option<serde_json::Value>) -> Self {
        self.response_body_json = response_body_json;
        self
    }
    pub fn request_headers(
        mut self,
        request_headers: Option<std::collections::BTreeMap<String, String>>,
    ) -> Self {
        self.request_headers = request_headers;
        self
    }
    pub fn response_headers(
        mut self,
        response_headers: Option<std::collections::BTreeMap<String, String>>,
    ) -> Self {
        self.response_headers = response_headers;
        self
    }
    pub fn is_streaming(mut self, is_streaming: bool) -> Self {
        self.is_streaming = is_streaming;
        self
    }
    pub fn stream_complete(mut self, stream_complete: bool) -> Self {
        self.stream_complete = stream_complete;
        self
    }
    pub fn stop_reason(mut self, stop_reason: Option<String>) -> Self {
        self.stop_reason = stop_reason;
        self
    }
    pub fn proxy_url(mut self, proxy_url: Option<String>) -> Self {
        self.proxy_url = proxy_url;
        self
    }
    pub fn proxy_status(mut self, proxy_status: Option<String>) -> Self {
        self.proxy_status = proxy_status;
        self
    }
    pub fn is_proxy_rotated(mut self, is_proxy_rotated: bool) -> Self {
        self.is_proxy_rotated = is_proxy_rotated;
        self
    }

    fn compute_prompt_tokens(&self) -> (Option<u32>, bool) {
        match self.prompt_tokens {
            Some(t) if t > 0 => (Some(t), false),
            _ => {
                let est = openproxy_compression::token_estimate::estimate_prompt_tokens(
                    &self.req.openai_request.messages,
                );
                if est > 0 {
                    tracing::debug!(
                        request_id = %self.req.request_id,
                        estimated_prompt_tokens = est,
                        "upstream did not report usage; estimated prompt tokens from request messages"
                    );
                    (Some(est), true)
                } else {
                    (None, false)
                }
            }
        }
    }

    fn compute_completion_tokens(&self) -> (Option<u32>, bool) {
        match self.completion_tokens {
            Some(t) if t > 0 => (Some(t), false),
            _ => {
                let completion_text = self
                    .response_body_json
                    .as_ref()
                    .and_then(|v| v.pointer("/choices/0/message/content"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                if completion_text.is_empty() {
                    (None, false)
                } else {
                    let est = openproxy_compression::token_estimate::estimate_completion_tokens(
                        completion_text,
                    );
                    tracing::debug!(
                        request_id = %self.req.request_id,
                        estimated_completion_tokens = est,
                        "upstream did not report usage; estimated completion tokens from response body"
                    );
                    (Some(est), true)
                }
            }
        }
    }

    fn emit_record_stage_event(&self) {
        let stage_label = if self.err.is_none() {
            "completed"
        } else if self.req.race_cancelled {
            "cancelled"
        } else {
            "failed"
        };
        let error_str = self
            .err
            .map(|e| openproxy_db::cost::redact_error_msg(&e.to_string()).0);
        openproxy_types::emit_stage_event!(
            request_id: self.req.request_id,
            trace_id: self.trace_id,
            stage: stage_label,
            elapsed_ms: self.total_ms,
            connect_ms: self.connect_ms,
            ttft_ms: self.ttft_ms,
            status_code: self.status_code,
            error: error_str,
            stop_reason: self.stop_reason.clone(),
        );
    }

    fn dispatch_record_job(&self, input: UsageInput) {
        let err_msg = self.err.map(std::string::ToString::to_string);
        let is_health_issue = self.err.is_some_and(is_upstream_health_issue);

        let job = crate::worker::BackgroundJob::RecordAttempt {
            usage_input: Box::new(input),
            target_id: self.target.id,
            combo_id: self.combo.id,
            error_msg: err_msg,
            is_upstream_health_issue: is_health_issue,
            cooldown_mode: self
                .target
                .cooldown_mode
                .unwrap_or(self.combo.cooldown_mode),
            cooldown_base_secs: self
                .target
                .cooldown_base_secs
                .or(self.combo.cooldown_base_secs)
                .unwrap_or(self.tracker.cooldown_secs),
            cooldown_max_secs: self
                .target
                .cooldown_max_secs
                .or(self.combo.cooldown_max_secs)
                .unwrap_or(self.tracker.cooldown_max_secs),
            cooldown_factor: self
                .target
                .cooldown_factor
                .or(self.combo.cooldown_factor)
                .unwrap_or(self.tracker.cooldown_factor),
        };

        if let Err(e) = self.tracker.background_tx.try_send(job) {
            if matches!(e, tokio::sync::mpsc::error::TrySendError::Closed(_)) {
                let job = e.into_inner();
                let conn = Arc::clone(&self.tracker.conn);
                let repo = Arc::clone(&self.tracker.repo);
                let selection_registry = Arc::clone(&self.tracker.selection_registry);
                drop(tokio::task::spawn_blocking(move || {
                    crate::worker::process_job(&conn, repo.as_ref(), job, &selection_registry);
                }));
            } else {
                tracing::warn!("failed to send RecordAttempt to background worker: {}", e);
            }
        }
    }
}

fn resolve_recorded_request_body(
    recording: bool,
    req_body: Option<bytes::Bytes>,
    openai_req: &openproxy_types::OpenAIRequest,
) -> Option<bytes::Bytes> {
    if !recording {
        return None;
    }
    req_body.or_else(|| serde_json::to_vec(openai_req).ok().map(bytes::Bytes::from))
}

fn optional_when_recording<T>(recording: bool, val: Option<T>) -> Option<T> {
    if recording { val } else { None }
}

impl UsageRecordBuilder<'_> {
    fn build_usage_input(
        &self,
        prompt_tokens: Option<u32>,
        prompt_tokens_estimated: bool,
        completion_tokens: Option<u32>,
        completion_tokens_estimated: bool,
        compression_savings_pct: Option<f64>,
        compression_techniques: Option<String>,
    ) -> UsageInput {
        let recording = self.tracker.is_recording();
        let request_body_json = resolve_recorded_request_body(
            recording,
            self.req.request_body_json.clone(),
            &self.req.openai_request,
        );
        let response_body_json =
            optional_when_recording(recording, self.response_body_json.clone());
        let request_headers = optional_when_recording(recording, self.request_headers.clone());
        let response_headers = optional_when_recording(recording, self.response_headers.clone());

        let mut flags = 0u8;
        if self.err.is_some() && self.req.race_cancelled {
            flags |= USAGE_FLAG_RACE_LOST;
        }
        if self.is_streaming {
            flags |= USAGE_FLAG_IS_STREAMING;
        }
        if self.stream_complete {
            flags |= USAGE_FLAG_STREAM_COMPLETE;
        }
        if prompt_tokens_estimated {
            flags |= USAGE_FLAG_PROMPT_ESTIMATED;
        }
        if completion_tokens_estimated {
            flags |= USAGE_FLAG_COMPLETION_ESTIMATED;
        }
        if self.is_proxy_rotated {
            flags |= USAGE_FLAG_PROXY_ROTATED;
        }

        UsageInput {
            request_id: self.req.request_id,
            trace_id: self.trace_id.clone(),
            attempt: self.attempt,
            provider_id: self.target.provider_id.clone(),
            account_id: self.target.account_id,
            combo_id: Some(self.combo.id),
            combo_target_id: Some(self.target.id),
            model_row_id: self.model.map(|m| m.row_id),
            upstream_model_id: self
                .model
                .map(|m| m.model_id.as_str().to_string())
                .unwrap_or_default(),
            prompt_tokens,
            completion_tokens,
            cached_tokens: self.cached_tokens,
            connect_ms: self.connect_ms,
            ttft_ms: self.ttft_ms,
            total_ms: self.total_ms,
            status_code: self.status_code,
            error_msg: self.err.map(|e| format!("{e}")),
            race_total: self.total_targets,
            api_key_id: self.req.api_key_id,
            request_body_json,
            response_body_json,
            request_headers,
            response_headers,
            error_message: self.err.map(|e| format!("{e}")),
            race_attempts: self.race_size,
            stop_reason: self.stop_reason.clone(),
            compression_savings_pct,
            compression_techniques,
            flags,
            endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
            proxy_url: self.proxy_url.clone(),
            proxy_status: self.proxy_status.clone(),
        }
    }

    fn update_selection_registry(&self) {
        if self.err.is_none() {
            self.tracker
                .selection_registry
                .record_success(self.target.id);
        } else {
            self.tracker
                .selection_registry
                .record_failure(self.target.id);
        }
    }

    pub fn record(
        self,
    ) -> Result<
        Option<(
            openproxy_types::ids::RequestId,
            u8,
            openproxy_types::ids::ComboTargetId,
        )>,
    > {
        let (compression_savings_pct, compression_techniques) = {
            let guard = self.tracker.compression_stats_cell.read();
            (
                guard
                    .as_ref()
                    .and_then(openproxy_compression::CompressionStats::savings_pct_opt),
                guard
                    .as_ref()
                    .and_then(openproxy_compression::CompressionStats::techniques_csv),
            )
        };

        let (prompt_tokens, prompt_tokens_estimated) = self.compute_prompt_tokens();
        let (completion_tokens, completion_tokens_estimated) = self.compute_completion_tokens();

        let input = self.build_usage_input(
            prompt_tokens,
            prompt_tokens_estimated,
            completion_tokens,
            completion_tokens_estimated,
            compression_savings_pct,
            compression_techniques,
        );

        self.emit_record_stage_event();
        self.dispatch_record_job(input);
        self.update_selection_registry();

        Ok(Some((self.req.request_id, self.attempt, self.target.id)))
    }
}
