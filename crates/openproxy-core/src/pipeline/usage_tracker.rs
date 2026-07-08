use parking_lot::{Mutex, RwLock};
use rusqlite::Connection;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::combos::{Combo, ComboTarget, SelectionRegistry};
use crate::compression::stats::CompressionStats;
use crate::cost::UsageInput;
use crate::error::{CoreError, Result};
use crate::models::Model;
use crate::pipeline::{FailureContext, PipelineRequest, PipelineResult, is_upstream_health_issue};

#[derive(Clone)]
pub struct UsageTracker {
    pub conn: Arc<Mutex<Connection>>,
    pub background_tx: tokio::sync::mpsc::Sender<crate::pipeline::worker::BackgroundJob>,
    pub record_bodies_and_headers: Arc<AtomicBool>,
    pub compression_stats_cell: Arc<RwLock<Option<CompressionStats>>>,
    pub selection_registry: Arc<SelectionRegistry>,
    pub cooldown_secs: u64,
    pub cooldown_max_secs: u64,
    pub cooldown_factor: u32,
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
        usage_tuple: Option<(String, u8, crate::ids::ComboTargetId)>,
    ) {
        let Some((request_id, attempt, target_id)) = usage_tuple else {
            return;
        };
        let job = crate::pipeline::worker::BackgroundJob::MarkClientResponse {
            request_id,
            attempt,
            target_id,
        };
        if let Err(e) = self.background_tx.try_send(job) {
            if matches!(e, tokio::sync::mpsc::error::TrySendError::Closed(_)) {
                let job = e.into_inner();
                let conn = self.conn.clone();
                crate::pipeline::worker::process_job(&conn, job);
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
        req: PipelineRequest,
        combo: &Combo,
        started: std::time::Instant,
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

    // ponytail: [Demasiados argumentos] -> [Refactorizar a struct en el futuro]
    pub(crate) fn record_and_fail_with_trace_id_and_partial(
        &self,
        req: PipelineRequest,
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
        let request_body_json = req.request_body_json.clone().or_else(|| {
            serde_json::to_value(&req.openai_request).ok()
        });
        let request_headers = crate::redact::redact_btreemap_sensitive(req.request_headers.clone());
        let response_body_json: Option<serde_json::Value> =
            acc.filter(|a| !a.is_completely_empty()).map(|a| {
                let chunk_id_str = chunk_id.unwrap_or("partial");
                a.finish(chunk_id_str, created, model_name)
            });

        let is_streaming = req.stream_sink.is_some() || req.openai_request.stream;
        let stream_complete = false;

        let usage_tuple = match UsageRecordBuilder::new(self, req, combo, target)
            .model_opt(model)
            .err(&err)
            .connect_ms_opt(connect_ms)
            .ttft_ms_opt(ttft_ms)
            .total_ms(total_ms)
            .status_code(status_code)
            .attempt(attempt)
            .race_size(race_size)
            .trace_id(trace_id)
            .request_body_json(request_body_json)
            .response_body_json(response_body_json)
            .request_headers(Some(request_headers))
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
    pub(crate) trace_id: String,
    pub(crate) prompt_tokens: Option<u32>,
    pub(crate) completion_tokens: Option<u32>,
    pub(crate) request_body_json: Option<serde_json::Value>,
    pub(crate) response_body_json: Option<serde_json::Value>,
    pub(crate) request_headers: Option<std::collections::BTreeMap<String, String>>,
    pub(crate) response_headers: Option<std::collections::BTreeMap<String, String>>,
    pub(crate) is_streaming: bool,
    pub(crate) stream_complete: bool,
    pub(crate) stop_reason: Option<String>,
}

impl<'a> UsageRecordBuilder<'a> {
    pub fn new(
        tracker: &'a UsageTracker,
        req: PipelineRequest,
        combo: &'a Combo,
        target: &'a ComboTarget,
    ) -> Self {
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
            trace_id: "".to_string(),
            prompt_tokens: None,
            completion_tokens: None,
            request_body_json: None,
            response_body_json: None,
            request_headers: None,
            response_headers: None,
            is_streaming: false,
            stream_complete: false,
            stop_reason: None,
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
    pub fn trace_id(mut self, trace_id: String) -> Self {
        self.trace_id = trace_id;
        self
    }
    pub fn prompt_tokens_opt(mut self, prompt_tokens: Option<u32>) -> Self {
        self.prompt_tokens = prompt_tokens;
        self
    }
    pub fn completion_tokens_opt(mut self, completion_tokens: Option<u32>) -> Self {
        self.completion_tokens = completion_tokens;
        self
    }
    pub fn request_body_json(mut self, request_body_json: Option<serde_json::Value>) -> Self {
        self.request_body_json = request_body_json;
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

    pub fn record(self) -> Result<Option<(String, u8, crate::ids::ComboTargetId)>> {
        let recording = self.tracker.is_recording();
        let compression_stats_snapshot = self.tracker.compression_stats_cell.read().clone();
        let compression_savings_pct = compression_stats_snapshot
            .as_ref()
            .and_then(|s| s.savings_pct_opt());
        let compression_techniques = compression_stats_snapshot
            .as_ref()
            .and_then(|s| s.techniques_csv());

        let (prompt_tokens, prompt_tokens_estimated) = match self.prompt_tokens {
            Some(t) if t > 0 => (Some(t), false),
            _ => {
                let est = crate::token_estimate::estimate_prompt_tokens(
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
        };

        let (completion_tokens, completion_tokens_estimated) = match self.completion_tokens {
            Some(t) if t > 0 => (Some(t), false),
            _ => {
                let completion_text = self
                    .response_body_json
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
                        request_id = %self.req.request_id,
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
            connect_ms: self.connect_ms,
            ttft_ms: self.ttft_ms,
            total_ms: self.total_ms,
            status_code: self.status_code,
            error_msg: self.err.map(|e| format!("{}", e)),
            race_total: self.race_size,
            race_lost: self.err.is_some() && self.req.race_cancelled,
            api_key_id: self.req.api_key_id,
            request_body_json: if recording {
                self.request_body_json.clone()
            } else {
                None
            },
            response_body_json: if recording {
                self.response_body_json.clone()
            } else {
                None
            },
            request_headers: if recording {
                self.request_headers.clone()
            } else {
                None
            },
            response_headers: if recording {
                self.response_headers.clone()
            } else {
                None
            },
            error_message: self.err.map(|e| format!("{}", e)),
            race_attempts: self.race_size,
            is_streaming: self.is_streaming,
            stream_complete: self.stream_complete,
            stop_reason: self.stop_reason.clone(),
            compression_savings_pct,
            compression_techniques,
            client_response: false,
            prompt_tokens_estimated,
            completion_tokens_estimated,
            endpoint_kind: crate::endpoint::EndpointKind::Chat,
        };

        {
            let stage_label: &str = if self.err.is_none() {
                "completed"
            } else if self.req.race_cancelled {
                "cancelled"
            } else {
                "failed"
            };
            let error_str: Option<String> = self
                .err
                .map(|e| crate::cost::redact_error_msg(&e.to_string()).0);
            let terminal_snapshot = self.tracker.compression_stats_cell.read().clone();
            crate::usage::publish_stage_event(crate::usage::StageEvent {
                request_id: self.req.request_id.to_string(),
                trace_id: self.trace_id.to_string(),
                provider_id: self.target.provider_id.to_string(),
                upstream_model_id: self
                    .model
                    .map(|m| m.model_id.as_str().to_string())
                    .unwrap_or_default(),
                stage: stage_label.into(),
                elapsed_ms: self.total_ms,
                connect_ms: self.connect_ms,
                ttft_ms: self.ttft_ms,
                status_code: self.status_code,
                error: error_str,

                stop_reason: self.stop_reason.clone(),
                compression_savings_pct: terminal_snapshot
                    .as_ref()
                    .and_then(|s| s.savings_pct_opt()),
                compression_techniques: terminal_snapshot.as_ref().and_then(|s| s.techniques_csv()),
                timestamp: String::new(),
                endpoint_kind: crate::endpoint::EndpointKind::Chat,
            });
        }

        let err_msg = self.err.map(|e| e.to_string());
        let is_health_issue = if let Some(e) = self.err {
            is_upstream_health_issue(e)
        } else {
            false
        };

        let job = crate::pipeline::worker::BackgroundJob::RecordAttempt {
            usage_input: input,
            target_id: self.target.id,
            combo_id: self.combo.id,
            error_msg: err_msg,
            is_upstream_health_issue: is_health_issue,
            cooldown_mode: self.combo.cooldown_mode,
            cooldown_base_secs: self
                .combo
                .cooldown_base_secs
                .unwrap_or(self.tracker.cooldown_secs),
            cooldown_max_secs: self
                .combo
                .cooldown_max_secs
                .unwrap_or(self.tracker.cooldown_max_secs),
            cooldown_factor: self
                .combo
                .cooldown_factor
                .unwrap_or(self.tracker.cooldown_factor),
        };

        if let Err(e) = self.tracker.background_tx.try_send(job) {
            if matches!(e, tokio::sync::mpsc::error::TrySendError::Closed(_)) {
                let job = e.into_inner();
                let conn = self.tracker.conn.clone();
                crate::pipeline::worker::process_job(&conn, job);
            } else {
                tracing::warn!("failed to send RecordAttempt to background worker: {}", e);
            }
        }

        self.tracker
            .selection_registry
            .record_request(self.target.id);
        if self.err.is_none() {
            self.tracker
                .selection_registry
                .record_success(self.target.id);
        }

        Ok(Some((
            self.req.request_id.to_string(),
            self.attempt,
            self.target.id,
        )))
    }
}
