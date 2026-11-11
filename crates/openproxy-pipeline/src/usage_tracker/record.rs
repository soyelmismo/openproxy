//! `UsageRecordBuilder`: the chained builder that accumulates the per-request
//! facts (timings, chunks gaps, status, bodies) and turns them into one
//! `usage_records` row via `send`. Split from `usage_tracker.rs` so the
//! tracker core and the record builder each stay under the LOC invariant.

use crate::PipelineRequest;
use crate::is_upstream_health_issue;
use crate::usage_tracker::UsageTracker;
use openproxy_types::combos::{Combo, ComboTarget};
use openproxy_types::error::{CoreError, Result};
use openproxy_types::models::Model;
use openproxy_types::usage::{
    USAGE_FLAG_CLIENT_RESPONSE, USAGE_FLAG_COMPLETION_ESTIMATED, USAGE_FLAG_IS_STREAMING,
    USAGE_FLAG_PROMPT_ESTIMATED, USAGE_FLAG_PROXY_ROTATED, USAGE_FLAG_RACE_LOST,
    USAGE_FLAG_STREAM_COMPLETE, UsageInput,
};

pub struct UsageRecordBuilder<'a> {
    pub(crate) tracker: &'a UsageTracker,
    pub(crate) req: PipelineRequest,
    pub(crate) combo: &'a Combo,
    pub(crate) target: &'a ComboTarget,
    pub(crate) model: Option<&'a Model>,
    pub(crate) err: Option<&'a CoreError>,
    pub(crate) connect_ms: Option<u64>,
    pub(crate) ttft_ms: Option<u64>,
    pub(crate) max_chunk_gap_ms: Option<u64>,
    pub(crate) avg_chunk_gap_ms: Option<u64>,
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
    pub(crate) client_response: bool,
    pub(crate) stop_reason: Option<String>,
    pub(crate) proxy_url: Option<String>,
    pub(crate) proxy_status: Option<String>,
    pub(crate) is_proxy_rotated: bool,
    pub(crate) redact_logs: bool,
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
            max_chunk_gap_ms: None,
            avg_chunk_gap_ms: None,
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
            client_response: false,
            stop_reason: None,
            proxy_url: None,
            proxy_status: None,
            is_proxy_rotated: false,
            redact_logs: true,
        }
    }

    pub fn redact_logs(mut self, redact: bool) -> Self {
        self.redact_logs = redact;
        self
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
    /// Sets the streaming inter-chunk gap telemetry (max/mean, ms). Only
    /// meaningful for completed streams; the learned idle-chunk budget
    /// consumes the recorded column, not this builder.
    pub fn chunk_gap_ms_opt(
        mut self,
        max_chunk_gap_ms: Option<u64>,
        avg_chunk_gap_ms: Option<u64>,
    ) -> Self {
        self.max_chunk_gap_ms = max_chunk_gap_ms;
        self.avg_chunk_gap_ms = avg_chunk_gap_ms;
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
    pub fn client_response(mut self, client_response: bool) -> Self {
        self.client_response = client_response;
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

    pub(crate) fn compute_completion_tokens(&self) -> (Option<u32>, bool) {
        match self.completion_tokens {
            Some(t) if t > 0 => (Some(t), false),
            _ => {
                let Some(ref body) = self.response_body_json else {
                    return (None, false);
                };
                let est =
                    openproxy_compression::token_estimate::estimate_completion_tokens_from_body(
                        body,
                    );
                if est > 0 {
                    tracing::debug!(
                        request_id = %self.req.request_id,
                        estimated_completion_tokens = est,
                        "upstream did not report usage; estimated completion tokens from response body"
                    );
                    (Some(est), true)
                } else {
                    (None, false)
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
            .map(|e| openproxy_db::cost::redact_error_msg(&e.to_string()));
        let pii_redacted = self
            .req
            .pii_session
            .lock()
            .as_ref()
            .and_then(|s| s.summary());
        openproxy_types::emit_stage_event!(
            request_id: self.req.request_id,
            trace_id: self.trace_id,
            stage: stage_label,
            elapsed_ms: self.total_ms,
            session_hash: crate::session_affinity::SessionAffinityRegistry::extract_session_hash(&self.req),
            connect_ms: self.connect_ms,
            ttft_ms: self.ttft_ms,
            status_code: self.status_code,
            error: error_str,
            stop_reason: self.stop_reason.clone(),
            pii_redacted: pii_redacted,
        );
    }

    async fn dispatch_record_job(&self, input: UsageInput) -> Result<()> {
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

        self.tracker.enqueue(job).await
    }
}

fn resolve_recorded_request_body(
    recording: bool,
    req_body: Option<bytes::Bytes>,
    openai_req: &openproxy_types::OpenAIRequest,
    prepared_msgs: Option<&[openproxy_types::OpenAIMessage]>,
    has_pii: bool,
    redact_logs: bool,
) -> Option<bytes::Bytes> {
    if !recording {
        return None;
    }
    if has_pii
        && redact_logs
        && let Some(msgs) = prepared_msgs
    {
        let mut cloned = openai_req.clone();
        cloned.messages = msgs.to_vec();
        if let Ok(vec) = serde_json::to_vec(&cloned) {
            return Some(bytes::Bytes::from(vec));
        }
    }
    req_body.or_else(|| serde_json::to_vec(openai_req).ok().map(bytes::Bytes::from))
}

fn optional_when_recording<T>(recording: bool, val: Option<T>) -> Option<T> {
    if recording { val } else { None }
}

impl<'a> UsageRecordBuilder<'a> {
    fn build_usage_input(
        &self,
        prompt_tokens: Option<u32>,
        prompt_tokens_estimated: bool,
        completion_tokens: Option<u32>,
        completion_tokens_estimated: bool,
        compression_savings_pct: Option<f64>,
        compression_techniques: Option<String>,
    ) -> UsageInput {
        let pii_redacted = self
            .req
            .pii_session
            .lock()
            .as_ref()
            .and_then(|s| s.summary());
        let has_pii = pii_redacted.is_some();
        let prepared_msgs = self
            .req
            .compressed_messages
            .get()
            .and_then(|opt| opt.as_deref());

        let recording = self.tracker.is_recording();
        let request_body_json = resolve_recorded_request_body(
            recording,
            self.req.request_body_json.clone(),
            &self.req.openai_request,
            prepared_msgs,
            has_pii,
            self.redact_logs,
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
        if self.client_response {
            flags |= USAGE_FLAG_CLIENT_RESPONSE;
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
            max_chunk_gap_ms: self.max_chunk_gap_ms,
            avg_chunk_gap_ms: self.avg_chunk_gap_ms,
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
            pii_redacted,
            flags,
            endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
            audio_ms: None,
            session_hash: crate::session_affinity::SessionAffinityRegistry::extract_session_hash(
                &self.req,
            ),
            proxy_url: self.proxy_url.clone(),
            proxy_status: self.proxy_status.clone(),
        }
    }

    fn update_selection_registry(&self) {
        let latency_ms = self.total_ms;
        if self.err.is_none() {
            self.tracker
                .selection_registry
                .record_success_with_latency(self.target.id, latency_ms);
        } else {
            let is_timeout = self.err.as_ref().is_some_and(|e| {
                matches!(
                    e,
                    openproxy_types::CoreError::UpstreamTimeout { .. }
                        | openproxy_types::CoreError::Cancelled(
                            openproxy_types::error::CancelReason::WatchdogTimeout
                        )
                )
            });
            self.tracker
                .selection_registry
                .record_failure_with_kind(self.target.id, is_timeout);
        }
    }

    /// Await bounded worker admission. Boxing this future keeps request/body
    /// state out of every enclosing pipeline-stage future.
    pub fn record(
        self,
    ) -> impl std::future::Future<
        Output = Result<
            Option<(
                openproxy_types::ids::RequestId,
                u8,
                openproxy_types::ids::ComboTargetId,
            )>,
        >,
    > + Send
    + 'a {
        Box::pin(async move {
            let (compression_savings_pct, compression_techniques) = {
                let guard = self.req.compression_stats.lock();
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
            self.update_selection_registry();
            self.dispatch_record_job(input).await?;

            Ok(Some((self.req.request_id, self.attempt, self.target.id)))
        })
    }
}
