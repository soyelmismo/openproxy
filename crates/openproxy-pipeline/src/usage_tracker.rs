use parking_lot::Mutex;
use rusqlite::Connection;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{FailureContext, PipelineRequest, PipelineResult};
use openproxy_types::SelectionRegistry;
use openproxy_types::combos::Combo;
use openproxy_types::error::Result;
use openproxy_types::usage::{USAGE_FLAG_CLIENT_RESPONSE, UsageInput};

#[derive(Clone)]
pub struct UsageTracker {
    pub conn: Arc<Mutex<Connection>>,
    pub background_tx: tokio::sync::mpsc::Sender<crate::worker::BackgroundJob>,
    pub record_bodies_and_headers: Arc<AtomicBool>,
    pub selection_registry: Arc<SelectionRegistry>,
    pub cooldown_secs: u64,
    pub cooldown_max_secs: u64,
    pub cooldown_factor: u32,
    pub repo: Arc<dyn crate::repository::PipelineRepository>,
    pub coordinator: Option<Arc<crate::worker::JournalCoordinator>>,
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
    pub fn with_coordinator(mut self, coordinator: Arc<crate::worker::JournalCoordinator>) -> Self {
        self.coordinator = Some(coordinator);
        self
    }

    pub fn is_recording(&self) -> bool {
        self.record_bodies_and_headers.load(Ordering::Relaxed)
    }

    pub fn set_recording(&self, enabled: bool) {
        self.record_bodies_and_headers
            .store(enabled, Ordering::Relaxed);
    }

    pub(crate) async fn mark_client_response(
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
        if let Err(error) = self.enqueue(job).await {
            tracing::error!(%error, "failed to mark client response");
        }
    }

    async fn enqueue(&self, job: crate::worker::BackgroundJob) -> Result<()> {
        crate::worker::enqueue_with_backpressure(
            &self.conn,
            &self.background_tx,
            job,
            self.coordinator.as_deref(),
        )
        .await
    }

    pub(crate) async fn record_no_healthy_targets_row(
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
            max_chunk_gap_ms: None,
            avg_chunk_gap_ms: None,
            total_ms: started.elapsed().as_millis() as u64,
            status_code: 502,
            error_msg: Some("no_healthy_targets".to_string()),
            race_total: 1,
            api_key_id: req.api_key_id,
            compression_savings_pct: None,
            compression_techniques: None,
            pii_redacted: None,
            request_body_json: None,
            response_body_json: None,
            request_headers: None,
            response_headers: None,
            error_message: Some("no_healthy_targets".to_string()),
            race_attempts: 1,
            stop_reason: None,
            flags: USAGE_FLAG_CLIENT_RESPONSE,
            endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
            audio_ms: None,
            session_hash: crate::session_affinity::SessionAffinityRegistry::extract_session_hash(
                req,
            ),
        };
        if let Err(error) = self
            .enqueue(crate::worker::BackgroundJob::RecordUsage(Box::new(input)))
            .await
        {
            tracing::error!(%error, "failed to record no healthy targets");
        }
    }

    pub(crate) async fn record_predictive_skipped_row(
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
            max_chunk_gap_ms: None,
            avg_chunk_gap_ms: None,
            total_ms: 0,
            status_code: 0,
            error_msg: Some("predict_skipped".to_string()),
            race_total: 1,
            api_key_id: req.api_key_id,
            compression_savings_pct: None,
            compression_techniques: None,
            pii_redacted: None,
            request_body_json: None,
            response_body_json: None,
            request_headers: None,
            response_headers: None,
            error_message: Some("predictive rate limit: skipped to avoid 429".to_string()),
            race_attempts: 1,
            stop_reason: None,
            flags: 0,
            endpoint_kind: req.endpoint_kind,
            audio_ms: None,
            session_hash: crate::session_affinity::SessionAffinityRegistry::extract_session_hash(
                req,
            ),
        };
        if let Err(error) = self
            .enqueue(crate::worker::BackgroundJob::RecordUsage(Box::new(input)))
            .await
        {
            tracing::error!(%error, "failed to record predictive skip");
        }
    }

    pub(crate) async fn record_and_fail_with_trace_id_and_partial(
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
                let val = a.finish(chunk_id_str, created, model_name);
                let think_extracted =
                    crate::think_extractor::extract_think_from_response_value(val);
                crate::inline_tools::extract_inline_tools_from_response_value(think_extracted)
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
            .client_response(false)
            .record()
            .await
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

pub(crate) mod record;

pub use record::UsageRecordBuilder;

#[cfg(test)]
#[path = "usage_tracker_tests.rs"]
mod tests;
