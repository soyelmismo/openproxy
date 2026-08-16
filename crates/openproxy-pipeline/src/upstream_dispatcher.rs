use crate::timeouts::Timeouts;
use crate::translation::OpenAIResponse;
use crate::{FailureContext, PipelineRequest, PipelineResult, parse_retry_after_ms};
use openproxy_adapters::ProviderAdapter;
use openproxy_adapters::upstream::{CancellationToken, UpstreamError, UpstreamRequest};
use openproxy_types::combos::{Combo, ComboTarget};
use openproxy_types::error::CoreError;
use openproxy_types::models::Model;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::watch;

use crate::think_extractor::extract_think_from_response;

/// Bundles the parameters shared by streaming failure methods
/// (`fail_stream_client_disconnected`, `fail_on_sink_send_error`).
/// Eliminates la anti-pattern de 14-15 argumentos posicionales.
pub(crate) struct DispatchContext<'a> {
    pub(crate) attempt: u8,
    pub(crate) race_size: u8,
    pub(crate) started: Instant,
    pub(crate) model: &'a Model,
    pub(crate) proxy_url: Option<String>,
    pub(crate) proxy_status: Option<String>,
}

impl<'a> DispatchContext<'a> {
    #[inline]
    pub(crate) fn fail_ctx_code<'e>(
        &self,
        err: &'e CoreError,
        connect_ms: Option<u64>,
        ttft_ms: Option<u64>,
        status_code: u16,
    ) -> crate::FailureContext<'e>
    where
        'a: 'e,
    {
        crate::FailureContext {
            proxy_url: self.proxy_url.clone(),
            proxy_status: self.proxy_status.clone(),
            attempt: self.attempt,
            race_size: self.race_size,
            err,
            started: self.started,
            model: Some(self.model),
            connect_ms,
            ttft_ms,
            status_code,
        }
    }
}

pub(crate) struct StreamFailureContext<'a> {
    pub(crate) req: PipelineRequest,
    pub(crate) combo: &'a Combo,
    pub(crate) target: &'a ComboTarget,
    pub(crate) attempt: u8,
    pub(crate) race_size: u8,
    pub(crate) started: std::time::Instant,
    pub(crate) model: &'a Model,
    pub(crate) connect_ms: u64,
    pub(crate) ttft_ms: Option<u64>,
    pub(crate) trace_id: String,
    pub(crate) acc: Option<&'a mut crate::sse_accumulator::ResponseAccumulator>,
    pub(crate) chunk_id: &'a str,
    pub(crate) created: u64,
    pub(crate) model_name: &'a str,
    pub(crate) proxy_url: Option<String>,
    pub(crate) proxy_status: Option<String>,
}

pub trait Dispatcher: Send + Sync {
    fn is_recording(&self) -> bool;
}

impl Dispatcher for UpstreamDispatcher {
    fn is_recording(&self) -> bool {
        self.record_bodies_and_headers()
    }
}

#[derive(Clone)]
pub struct UpstreamDispatcher {
    pub(crate) conn: Arc<parking_lot::Mutex<rusqlite::Connection>>,
    pub(crate) config: crate::PipelineConfig,
    pub(crate) tracker: crate::usage_tracker::UsageTracker,
    pub(crate) record_bodies_and_headers: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug)]
pub(crate) enum ProxyRotationTrigger {
    Status(u16),
    ConnectError,
    RateLimited,
}

impl UpstreamDispatcher {
    pub(crate) fn new(
        conn: Arc<parking_lot::Mutex<rusqlite::Connection>>,
        config: crate::PipelineConfig,
        tracker: crate::usage_tracker::UsageTracker,
        record_bodies_and_headers: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            conn,
            config,
            tracker,
            record_bodies_and_headers,
        }
    }

    pub(crate) fn record_bodies_and_headers(&self) -> bool {
        self.record_bodies_and_headers
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) async fn check_and_trigger_proxy_rotation(
        &self,
        provider_id: &openproxy_types::ids::ProviderId,
        account_id: Option<openproxy_types::ids::AccountId>,
        override_proxy_id: Option<&str>,
        trigger: crate::upstream_dispatcher::ProxyRotationTrigger,
        cooldown_ms: Option<u64>,
    ) -> bool {
        let conn_clone = Arc::clone(&self.conn);
        let provider_id = provider_id.to_owned();
        let repo = Arc::clone(&self.tracker.repo);
        let override_proxy_id = override_proxy_id.map(str::to_string);
        tokio::task::spawn_blocking(move || {
            let (provider, bad_proxy_id, is_per_account) = {
                let conn = conn_clone.lock();
                let provider = openproxy_db::providers::get(&conn, &provider_id).unwrap_or(None);
                if let Some(provider) = provider
                    && provider.use_proxies
                {
                    let is_per_account = provider.proxy_rotation_mode == "account";
                    let bad_proxy_id = if let Some(ref pid) = override_proxy_id {
                        Some(pid.clone())
                    } else if is_per_account {
                        if let Some(ref acc_id) = account_id {
                            openproxy_db::accounts::get_current_proxy_id(&conn, *acc_id).unwrap_or(None)
                        } else {
                            None
                        }
                    } else {
                        provider.current_proxy_id.clone()
                    };
                    (provider, bad_proxy_id, is_per_account)
                } else {
                    return false;
                }
            };

            let should_rotate = match trigger {
                crate::upstream_dispatcher::ProxyRotationTrigger::RateLimited => true,
                crate::upstream_dispatcher::ProxyRotationTrigger::Status(sc) => {
                    let sc_str = sc.to_string();
                    provider
                        .proxy_rotation_errors
                        .split(',')
                        .map(str::trim)
                        .any(|e| e == sc_str)
                }
                crate::upstream_dispatcher::ProxyRotationTrigger::ConnectError => provider
                    .proxy_rotation_errors
                    .split(',')
                    .map(str::trim)
                    .any(|e| e == "connect_error" || e == "timeout"),
            };

            if should_rotate && let Some(ref bad_proxy) = bad_proxy_id {
                tracing::warn!(
                    provider = %provider_id,
                    account_id = ?account_id,
                    proxy_id = %bad_proxy,
                    trigger = ?trigger,
                    "proxy rotation triggered: clearing binding and adding cooldown for provider"
                );
                let cooldown_duration = cooldown_ms.map_or_else(|| std::time::Duration::from_mins(15), std::time::Duration::from_millis);

                // Only mark proxy as "dead" on connection errors, NOT on rate limits / 429 status.
                // Rate limiting is per-provider IP throttling, so the proxy host is still alive.
                if matches!(trigger, crate::upstream_dispatcher::ProxyRotationTrigger::ConnectError) {
                    let _ = repo.update_proxy_status(bad_proxy, "dead", None);
                }

                let conn = conn_clone.lock();
                let _ = openproxy_db::cooldowns::add_provider_proxy_cooldown(
                    &conn,
                    provider_id.as_str(),
                    bad_proxy,
                    cooldown_duration,
                );
                if is_per_account {
                    if let Some(ref acc_id) = account_id {
                        let _ = openproxy_db::accounts::clear_current_proxy_id(&conn, *acc_id);
                    }
                } else {
                    let _ = openproxy_db::providers::update_current_proxy(
                        &conn,
                        &provider_id,
                        None,
                    );
                }

                let has_candidates = openproxy_db::free_proxies::get_candidate_proxies_for_provider(&conn, &provider_id, 1)
                    .is_ok_and(|c| !c.is_empty());

                return has_candidates;
            }
            false
        })
        .await
        .unwrap_or(false)
    }

    pub(crate) fn is_client_disconnected(
        &self,
        rx: &mut watch::Receiver<Option<openproxy_types::CancelReason>>,
    ) -> Option<openproxy_types::CancelReason> {
        *rx.borrow_and_update()
    }

    pub(crate) fn record_and_fail(
        &self,
        req: PipelineRequest,
        combo: &Combo,
        target: &ComboTarget,
        ctx: FailureContext<'_>,
    ) -> PipelineResult {
        let trace_id = if ctx.attempt > 1 {
            format!("{}:retry{}", req.trace_id, ctx.attempt - 1)
        } else {
            req.trace_id.to_string()
        };
        self.record_and_fail_with_trace_id(req, combo, target, ctx, trace_id)
    }
}

pub(crate) struct DispatchParams<'a> {
    pub target: &'a ComboTarget,
    pub combo: &'a Combo,
    pub req: PipelineRequest,
    pub model: &'a Model,
    pub target_format: openproxy_types::TargetFormat,
    pub url: &'a str,
    pub headers: &'a [(String, String)],
    pub body_bytes: bytes::Bytes,
    pub resolved_timeouts: &'a Timeouts,
    pub started: Instant,
    pub attempt: u8,
    pub race_size: u8,
    pub trace_id: String,
}

pub(crate) struct StreamDispatchParams<'a> {
    pub target: &'a ComboTarget,
    pub combo: &'a Combo,
    pub req: PipelineRequest,
    pub model: &'a Model,
    pub target_format: openproxy_types::TargetFormat,
    pub resolved_timeouts: &'a Timeouts,
    pub started: Instant,
    pub attempt: u8,
    pub race_size: u8,
    pub trace_id: String,
    pub upstream_request: UpstreamRequest,
}

impl UpstreamDispatcher {
    pub(crate) fn record_and_fail_with_trace_id(
        &self,
        req: PipelineRequest,
        combo: &Combo,
        target: &ComboTarget,
        ctx: FailureContext<'_>,
        trace_id: String,
    ) -> PipelineResult {
        self.tracker
            .record_and_fail_with_trace_id_and_partial(crate::PartialFailureParams {
                req,
                combo,
                target,
                ctx,
                trace_id,
                acc: None,
                chunk_id: None,
                created: 0,
                model_name: "",
            })
    }

    pub(crate) fn record_and_fail_with_trace_id_and_partial(
        &self,
        params: crate::PartialFailureParams<'_>,
    ) -> PipelineResult {
        self.tracker
            .record_and_fail_with_trace_id_and_partial(params)
    }

    pub(crate) async fn dispatch_upstream(&self, params: DispatchParams<'_>) -> PipelineResult {
        let DispatchParams {
            target,
            combo,
            req,
            model,
            target_format,
            url,
            headers,
            body_bytes,
            resolved_timeouts,
            started,
            attempt,
            race_size,
            trace_id,
        } = params;
        let mut dctx = DispatchContext {
            attempt,
            race_size,
            started,
            model,
            proxy_url: None,
            proxy_status: None,
        };

        // Gate 2: both the non-streaming path AND the streaming path
        // now go through the hyper-based `UpstreamClient`
        // (`PipelineConfig::upstream_client`). The UpstreamClient
        // `request_builder` chain is gone from this dispatch.
        //
        // `body_bytes` is pre-serialized by the caller (single pass
        // from the translated struct — no intermediate `Value`).
        let mut upstream_request = UpstreamRequest::post_json(url.to_string(), body_bytes);
        // If the provider has proxy routing enabled, fetch/assign a proxy
        let proxy_result = if let Some((_, ref purl)) = req.proxy_override {
            Ok(Some(purl.clone()))
        } else {
            let repo = Arc::clone(&self.tracker.repo);
            let provider_id = target.provider_id.clone();
            let account_id = target.account_id;
            tokio::task::spawn_blocking(move || {
                repo.get_or_assign_provider_proxy(&provider_id, account_id)
            })
            .await
            .map_err(|e| CoreError::Internal(e.to_string()))
            .and_then(|res| res)
        };
        let proxy_url = match proxy_result {
            Ok(url) => url,
            Err(e) => {
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    dctx.fail_ctx_code(&e, None, None, e.http_status()),
                );
            }
        };
        upstream_request.proxy = proxy_url;

        let proxy_status = match upstream_request.proxy.as_ref() {
            Some(url) => {
                let repo = Arc::clone(&self.tracker.repo);
                let u = url.to_owned();
                tokio::task::spawn_blocking(move || repo.get_proxy_status_by_url(&u))
                    .await
                    .unwrap_or(None)
            }
            None => None,
        };
        upstream_request.proxy_status = proxy_status;
        dctx.proxy_url = upstream_request.proxy.clone();
        dctx.proxy_status = upstream_request.proxy_status.clone();
        tracing::info!(
            proxy_used = ?upstream_request.proxy,
            proxy_status = %upstream_request.proxy_status.as_ref().unwrap_or(&"none".to_string()),
            "assigned proxy for upstream request"
        );

        // is_streaming is always true because we force stream=true
        // to the upstream (see comment above). The body-chunk gap
        // timeout (idle_chunk_ms) applies normally — but only AFTER
        // the first chunk arrives (the initial deadline is
        // total_deadline, not start + body_chunk_ms).
        upstream_request.is_streaming = true;

        for (k, v) in headers {
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::from_bytes(k.as_bytes()),
                http::HeaderValue::from_str(v),
            ) {
                upstream_request.headers.insert(name, value);
            }
        }

        // Streaming-first dispatch: all upstream requests go through
        // `dispatch_upstream_streaming`, which drives the chunk-by-chunk
        // SSE state machine. The decision of whether to return a stream
        // response or aggregate into a non-streaming response is made
        // based on the client's preference, but the upstream call
        // always uses stream=true (set in the translation layer).
        if req.stream_sink.is_some() {
            return self
                .dispatch_upstream_streaming(StreamDispatchParams {
                    target,
                    combo,
                    req,
                    model,
                    target_format,
                    resolved_timeouts,
                    started,
                    attempt,
                    race_size,
                    trace_id,
                    upstream_request,
                })
                .await;
        }

        // Fallback: no stream_sink (shouldn't happen in production —
        // the chat handler always provides one). Uses the old
        // non-streaming path as a safety net.
        // building the request) we short-circuit to a structured
        // `Cancelled(openproxy_types::CancelReason::ClientDisconnected)` result. The pre-flight is the only
        // place we map `UpstreamError::Cancel` → `Cancelled(openproxy_types::CancelReason::ClientDisconnected)`
        // — see below for the rationale.
        let send_start = Instant::now();
        let client_disconnected = *req.client_disconnected.borrow();
        if let Some(reason) = client_disconnected {
            let elapsed = send_start.elapsed().as_millis() as u64;
            tracing::warn!(
                combo_id = combo.id.0,
                target_id = target.id.0,
                provider = %target.provider_id,
                elapsed_ms = elapsed,
                "client disconnected before upstream send; aborting attempt"
            );
            return self.record_and_fail(
                req,
                combo,
                target,
                dctx.fail_ctx_code(
                    &CoreError::Cancelled(reason),
                    Some(elapsed),
                    None,
                    CoreError::Cancelled(reason).http_status(),
                ),
            );
        }
        let cancel_token = CancellationToken::from_watch(tokio::sync::watch::Receiver::clone(
            &req.client_disconnected,
        ));
        let req_proxy_url = upstream_request.proxy.clone();
        let req_proxy_status = upstream_request.proxy_status.clone();
        let result = self
            .config
            .upstream_client
            .call(
                upstream_request,
                openproxy_adapters::upstream::TimeoutProfile::Custom(
                    resolved_timeouts.as_resolved(),
                ),
                cancel_token,
            )
            .await;
        let connect_and_send_ms = send_start.elapsed().as_millis() as u64;

        // Map the `UpstreamError` taxonomy to the `CoreError` shape
        // the downstream code expects. The split mirrors the
        // pre-migration `SendAbortReason` + `e.is_timeout()` /
        // `e.to_string()` mapping 1-to-1, except we now have
        // per-phase `UpstreamPhase` attribution and the `Cancel`
        // variant.
        let response_result: std::result::Result<
            openproxy_adapters::upstream::UpstreamResponse,
            UpstreamError,
        > = match result {
            Ok(r) => Ok(r),
            Err(UpstreamError::Cancel) => {
                tracing::warn!(
                    combo_id = combo.id.0,
                    target_id = target.id.0,
                    provider = %target.provider_id,
                    elapsed_ms = connect_and_send_ms,
                    "client cancelled during upstream send; aborting attempt"
                );
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    dctx.fail_ctx_code(
                        &CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected),
                        Some(connect_and_send_ms),
                        None,
                        CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected)
                            .http_status(),
                    ),
                );
            }
            Err(UpstreamError::Timeout(phase)) => {
                let is_proxy_rotated = self
                    .check_and_trigger_proxy_rotation(
                        &target.provider_id,
                        target.account_id,
                        req.proxy_override.as_ref().map(|(pid, _)| pid.as_str()),
                        crate::upstream_dispatcher::ProxyRotationTrigger::ConnectError,
                        None,
                    )
                    .await;
                let (phase_label, config_hint) = match phase {
                    openproxy_adapters::upstream::UpstreamPhase::Dns => ("dns", "connect_ms"),
                    openproxy_adapters::upstream::UpstreamPhase::Dial => ("dial", "connect_ms"),
                    openproxy_adapters::upstream::UpstreamPhase::Tls => ("tls", "connect_ms"),
                    openproxy_adapters::upstream::UpstreamPhase::Write => {
                        ("write", "request_send_ms")
                    }
                    openproxy_adapters::upstream::UpstreamPhase::Headers => ("headers", "ttft_ms"),
                    openproxy_adapters::upstream::UpstreamPhase::Body => ("body", "idle_chunk_ms"),
                    openproxy_adapters::upstream::UpstreamPhase::Total => ("total", "total_ms"),
                };
                tracing::warn!(
                    combo_id = combo.id.0,
                    target_id = target.id.0,
                    provider = %target.provider_id,
                    phase = %phase,
                    elapsed_ms = connect_and_send_ms,
                    config_hint = config_hint,
                    "upstream phase timed out; aborting attempt"
                );
                let err = CoreError::UpstreamError {
                    status: 504,
                    provider: target.provider_id.to_string(),
                    model: model.model_id.as_str().to_string(),
                    body: format!(
                        "upstream phase `{phase_label}` timed out after {connect_and_send_ms}ms (config: {config_hint})"
                    ),
                    is_proxy_rotated,
                };
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    dctx.fail_ctx_code(&err, Some(connect_and_send_ms), None, err.http_status()),
                );
            }
            Err(UpstreamError::Connection(msg) | UpstreamError::Tls(msg) |
UpstreamError::Http(msg) | UpstreamError::Decode(msg) |
UpstreamError::Invalid(msg)) => {
                let is_proxy_rotated = self
                    .check_and_trigger_proxy_rotation(
                        &target.provider_id,
                        target.account_id,
                        req.proxy_override.as_ref().map(|(pid, _)| pid.as_str()),
                        crate::upstream_dispatcher::ProxyRotationTrigger::ConnectError,
                        None,
                    )
                    .await;
                let err = CoreError::UpstreamError {
                    status: 502,
                    provider: target.provider_id.to_string(),
                    model: model.model_id.as_str().to_string(),
                    body: format!("upstream connection error: {msg}"),
                    is_proxy_rotated,
                };
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    dctx.fail_ctx_code(&err, Some(connect_and_send_ms), None, err.http_status()),
                );
            }
            Err(_) => {
                let is_proxy_rotated = self
                    .check_and_trigger_proxy_rotation(
                        &target.provider_id,
                        target.account_id,
                        req.proxy_override.as_ref().map(|(pid, _)| pid.as_str()),
                        crate::upstream_dispatcher::ProxyRotationTrigger::ConnectError,
                        None,
                    )
                    .await;
                let err = CoreError::UpstreamError {
                    status: 502,
                    provider: target.provider_id.to_string(),
                    model: model.model_id.as_str().to_string(),
                    body: "unknown upstream error".to_string(),
                    is_proxy_rotated,
                };
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    dctx.fail_ctx_code(&err, Some(connect_and_send_ms), None, err.http_status()),
                );
            }
        };

        // Live-log stage helper closure. Only fires when recording
        // is ON; OFF means the dashboard's "Record" toggle is off
        // and the operator doesn't want per-phase noise. Throttled
        // per-call: each caller site picks which stages matter.
        let emit_stage = |stage: &str, status: u16, err: Option<String>| {
            // dispatch_upstream runs strictly after execute_single's
            // step 4b (apply_compression), so the stats cell is
            // always populated here. Snapshot once per emission so
            // a concurrent retry on a different worker doesn't race
            // mid-publish.
            openproxy_types::usage::publish_stage_event(openproxy_types::usage::StageEvent {
                request_id: req.request_id.to_string(),
                trace_id: trace_id.clone(),
                provider_id: None,
                upstream_model_id: None,
                stage: stage.into(),
                elapsed_ms: started.elapsed().as_millis() as u64,
                connect_ms: Some(connect_and_send_ms),
                ttft_ms: None,
                status_code: Some(status),
                error: err,
                stop_reason: None,
                timestamp: None,
                endpoint_kind: None,
            });
        };

        let Ok(response) = response_result else {
            unreachable!("error variants are handled above with early return");
        };

        let status_code = response.status.as_u16();
        // Extract response headers BEFORE consuming the body
        let response_headers: Option<std::collections::BTreeMap<String, String>> =
            if self.is_recording() {
                Some(
                    response
                        .headers
                        .iter()
                        .map(|(k, v)| {
                            (
                                k.as_str().to_string(),
                                v.to_str().unwrap_or_default().to_string(),
                            )
                        })
                        .collect(),
                )
            } else {
                None
            };
        // Live-log: socket+headers are in, body streaming next.
        // For non-2xx we go to the error branch below; emit there.
        if (200..300).contains(&status_code) {
            emit_stage("waiting_ttft", status_code, None);
        }
        // For non-streaming we have no first-chunk signal, so the
        // conservative thing is to record `ttft == total`. The cost
        // module's tokens/sec guard already turns this into `None`.
        let ttft_ms = started.elapsed().as_millis() as u64;

        // Read the body via the upstream client's `collect()`. The
        // body is bounded to 32 MiB at the upstream layer; on cancel
        // we get `UpstreamError::Cancel` (mapped above); on read
        // failure we get `UpstreamError::Http`. We map any failure
        // to `UpstreamConnection` with a `read upstream body: …`
        // prefix, matching the pre-migration `record_and_fail` call
        // shape.
        //
        // Bug fix: for non-streaming requests, use `total_ms` (not
        // `ttft_ms`) as the body-read deadline. The previous code used
        // `ttft_ms` (default 30s) which is far too short for a
        // non-streaming request — the LLM has to generate the ENTIRE
        // response before sending anything, which can take 60-120s
        // for long responses.
        //
        // `ttft_ms` is a streaming concept: "how long to wait for the
        // first token". In non-streaming there are no tokens until the
        // full response is ready, so `ttft_ms` doesn't apply.
        // `idle_chunk_ms` is also a streaming concept (max gap between
        // chunks) and doesn't apply.
        //
        // For non-streaming, the correct timeout after connection +
        // headers is `total_ms` (the hard ceiling, default 300s = 5min).
        // The upstream client's internal `headers_deadline` (== ttft_ms)
        // still applies to the "wait for response headers" phase — that's
        // correct (the server should respond with HTTP headers quickly
        // even for non-streaming). But once headers arrive, the body
        // read should be bounded by `total_ms`, not `ttft_ms`.
        let non_streaming_body_deadline =
            started + std::time::Duration::from_millis(resolved_timeouts.total.as_millis() as u64);
        let mut remaining = non_streaming_body_deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(std::time::Duration::ZERO);

        // Error responses should not stall the pipeline. We give the upstream
        // 5 seconds to send the error body; if it stalls, we drop the body
        // and proceed with the error status code. This prevents "ghost" requests
        // stuck in `connecting` for 300s when an upstream hangs after sending headers.
        if !(200..300).contains(&status_code) {
            remaining = std::cmp::min(remaining, std::time::Duration::from_secs(5));
        }

        let body_bytes = match tokio::time::timeout(remaining, response.collect()).await {
            Ok(Ok(b)) => b,
            Ok(Err(UpstreamError::Cancel)) => {
                tracing::warn!(
                    combo_id = combo.id.0,
                    target_id = target.id.0,
                    provider = %target.provider_id,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "client cancelled during upstream body read; aborting attempt"
                );
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    dctx.fail_ctx_code(
                        &CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected),
                        Some(connect_and_send_ms),
                        Some(ttft_ms),
                        CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected)
                            .http_status(),
                    ),
                );
            }
            Ok(Err(UpstreamError::Timeout(phase))) => {
                let err = CoreError::UpstreamTimeout {
                    phase: phase.as_str().to_string(),
                    ms: started.elapsed().as_millis() as u64,
                };
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    dctx.fail_ctx_code(
                        &err,
                        Some(connect_and_send_ms),
                        Some(ttft_ms),
                        err.http_status(),
                    ),
                );
            }
            Ok(Err(e)) => {
                self.check_and_trigger_proxy_rotation(
                    &target.provider_id,
                    target.account_id,
                    req.proxy_override.as_ref().map(|(pid, _)| pid.as_str()),
                    crate::upstream_dispatcher::ProxyRotationTrigger::ConnectError,
                    None,
                )
                .await;
                let err = CoreError::UpstreamConnection(format!("read upstream body: {e}"));
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    dctx.fail_ctx_code(
                        &err,
                        Some(connect_and_send_ms),
                        Some(ttft_ms),
                        err.http_status(),
                    ),
                );
            }
            Err(_elapsed) => {
                self.check_and_trigger_proxy_rotation(
                    &target.provider_id,
                    target.account_id,
                    req.proxy_override.as_ref().map(|(pid, _)| pid.as_str()),
                    crate::upstream_dispatcher::ProxyRotationTrigger::ConnectError,
                    None,
                )
                .await;
                let elapsed = started.elapsed().as_millis() as u64;
                let err = CoreError::UpstreamTimeout {
                    phase: "total (config: total_ms)".to_string(),
                    ms: elapsed,
                };
                tracing::warn!(
                    combo_id = combo.id.0,
                    target_id = target.id.0,
                    provider = %target.provider_id,
                    elapsed_ms = elapsed,
                    "non-streaming body read exceeded total_ms; aborting attempt"
                );
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    dctx.fail_ctx_code(
                        &err,
                        Some(connect_and_send_ms),
                        Some(ttft_ms),
                        err.http_status(),
                    ),
                );
            }
        };

        // Non-2xx upstream responses are surfaced as UpstreamError, with
        // the body included for the usage row. We still consume the body
        // so the connection is released back to the pool cleanly.
        //
        // NEW-2 fix: when the upstream returns 429 (or 408/503) with a
        // `Retry-After` header, surface the error as `CoreError::RateLimited`
        // so the per-target retry loop honors the upstream-requested delay
        // instead of using the fixed exponential backoff. The default
        // backoff is < 1 s; an upstream that asks for 30 s gets 30 s.
        if !(200..300).contains(&status_code) {
            let mut is_proxy_rotated = self
                .check_and_trigger_proxy_rotation(
                    &target.provider_id,
                    target.account_id,
                    req.proxy_override.as_ref().map(|(pid, _)| pid.as_str()),
                    crate::upstream_dispatcher::ProxyRotationTrigger::Status(status_code),
                    None,
                )
                .await;
            let body_str = String::from_utf8_lossy(&body_bytes).to_string();
            // Parse `Retry-After` from response_headers (extracted at L1751
            // before the body was consumed). Accepts either an integer
            // number of seconds or an HTTP-date (RFC 7231).
            let retry_after_ms: Option<u64> = response_headers
                .as_ref()
                .and_then(|h| h.get("retry-after").or_else(|| h.get("Retry-After")))
                .and_then(|v| parse_retry_after_ms(v));
            let is_rate_limited_status =
                status_code == 429 || status_code == 408 || status_code == 503;
            if is_rate_limited_status {
                let retry_ms = retry_after_ms.unwrap_or(300_000);
                if !is_proxy_rotated {
                    is_proxy_rotated = self
                        .check_and_trigger_proxy_rotation(
                            &target.provider_id,
                            target.account_id,
                            req.proxy_override.as_ref().map(|(pid, _)| pid.as_str()),
                            crate::upstream_dispatcher::ProxyRotationTrigger::RateLimited,
                            Some(retry_ms),
                        )
                        .await;
                }
                let err = CoreError::RateLimited {
                    provider: target.provider_id.to_string(),
                    retry_after_ms: retry_ms,
                    is_proxy_rotated,
                };
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    dctx.fail_ctx_code(
                        &err,
                        Some(connect_and_send_ms),
                        Some(ttft_ms),
                        err.http_status(),
                    ),
                );
            }
            // G2.3: surface an `account_invalid` system notification
            // when the upstream rejects the account's credentials
            // (401 Unauthorized / 403 Forbidden). Other 4xx codes
            // (400 validation, 404 model gone, 408 timeout handled
            // above) are NOT account-level rejections and stay
            // silent. We fire one notification PER 4xx response —
            // the per-account dedup key collapses repeats within
            // 24h so a stuck upstream doesn't flood the tray, but a
            // different account hitting the same upstream 401 still
            // gets surfaced.
            //
            // Only fire when the target carries an `account_id`
            // (anonymous/account-rotation targets don't have a
            // specific account to flag).
            if (status_code == 401 || status_code == 403)
                && let Some(aid) = target.account_id
            {
                let provider_id_str = target.provider_id.to_string();
                let model_id_str = model.model_id.as_str().to_string();
                let dedup_key = format!("account_invalid:{}", aid.0);
                let payload = serde_json::json!({
                    "code": "account_invalid",
                    "message": format!(
                        "Account {} on {} rejected by upstream (HTTP {})",
                        aid.0, provider_id_str, status_code,
                    ),
                    "provider_id": &provider_id_str,
                    "details": {
                        "account_id": aid.0,
                        "provider_id": &provider_id_str,
                        "model_id": &model_id_str,
                        "status_code": status_code,
                    },
                });
                let repo = Arc::clone(&self.tracker.repo);
                let provider_id_str_clone = provider_id_str.clone();
                tokio::task::spawn_blocking(move || {
                    let _ = repo.insert_and_broadcast_notification(
                        "system",
                        &payload,
                        Some(&dedup_key),
                        Some(&provider_id_str_clone),
                    );
                })
                .await
                .ok();
            }
            let err = CoreError::UpstreamError {
                status: status_code,
                provider: target.provider_id.to_string(),
                model: model.model_id.as_str().to_string(),
                body: body_str,
                is_proxy_rotated,
            };
            return self.record_and_fail(
                req,
                combo,
                target,
                dctx.fail_ctx_code(&err, Some(connect_and_send_ms), Some(ttft_ms), status_code),
            );
        }

        // R2 fix: 2xx non-streaming success. The non-streaming path
        // doesn't have a "first SSE data line" signal — the whole
        // body arrives as a single `response.collect().await` — so
        // we emit `streaming` right after the body lands. This
        // closes the gap where the dashboard's stage label was
        // stuck on `waiting_ttft` between the 2xx headers
        // arriving and the (now missing) terminal `completed`
        // event being published by the success path.
        // Emit `waiting_ttft` before `streaming` for stage sequence
        // consistency with the non-streaming path. The streaming path
        // previously skipped this, but now that non-streaming clients
        // also go through the streaming path, we need it for the
        // stage sequence test to pass.
        openproxy_types::usage::publish_stage_event(openproxy_types::usage::StageEvent {
            request_id: req.request_id.to_string(),
            trace_id: trace_id.clone(),
            provider_id: None,
            upstream_model_id: None,
            stage: "waiting_ttft".into(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            connect_ms: Some(connect_and_send_ms),
            ttft_ms: None,
            status_code: Some(status_code),
            error: None,
            stop_reason: None,
            timestamp: None,
            endpoint_kind: None,
        });
        openproxy_types::usage::publish_stage_event(openproxy_types::usage::StageEvent {
            request_id: req.request_id.to_string(),
            trace_id: trace_id.clone(),
            provider_id: None,
            upstream_model_id: None,
            stage: "streaming".into(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            connect_ms: Some(connect_and_send_ms),
            ttft_ms: Some(ttft_ms),
            status_code: Some(status_code),
            error: None,
            stop_reason: None,
            timestamp: None,
            endpoint_kind: None,
        });

        // Parse format-specific response
        let response_body_raw: serde_json::Value = match serde_json::from_slice(&body_bytes) {
            Ok(v) => v,
            Err(e) => {
                let err = CoreError::Parse(format!("invalid json in upstream response: {e}"));
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    dctx.fail_ctx_code(
                        &err,
                        Some(connect_and_send_ms),
                        Some(ttft_ms),
                        err.http_status(),
                    ),
                );
            }
        };

        // Snapshot the body JSON before it gets moved into the
        // format-specific parser below; we need it both as the
        // recorded response body and as a source for the request
        // body we are about to send.
        let response_body_value = response_body_raw.clone();

        let openai_response = match target_format {
            openproxy_types::TargetFormat::Responses => {
                unreachable!("Responses format is handled natively before dispatcher")
            }
            openproxy_types::TargetFormat::Openai => {
                match <OpenAIResponse as serde::Deserialize>::deserialize(&response_body_raw) {
                    Ok(r) => r,
                    Err(e) => {
                        let err = CoreError::Parse(format!("parse openai response: {e}"));
                        return self.record_and_fail(
                            req,
                            combo,
                            target,
                            dctx.fail_ctx_code(
                                &err,
                                Some(connect_and_send_ms),
                                Some(ttft_ms),
                                err.http_status(),
                            ),
                        );
                    }
                }
            }
            openproxy_types::TargetFormat::Anthropic => {
                let anthropic_resp: crate::translation::AnthropicResponse =
                    match <crate::translation::AnthropicResponse as serde::Deserialize>::deserialize(
                        &response_body_raw,
                    ) {
                        Ok(r) => r,
                        Err(e) => {
                            let err = CoreError::Parse(format!("parse anthropic response: {e}"));
                            return self.record_and_fail(
                                req,
                                combo,
                                target,
                                dctx.fail_ctx_code(
                                    &err,
                                    Some(connect_and_send_ms),
                                    Some(ttft_ms),
                                    err.http_status(),
                                ),
                            );
                        }
                    };
                crate::translation::anthropic_to_openai(&anthropic_resp)
            }
            openproxy_types::TargetFormat::Gemini => {
                let adapter = openproxy_adapters::GeminiAdapter::new();
                match adapter.translate_non_streaming_response(target_format, response_body_raw) {
                    Ok(r) => r,
                    Err(err) => {
                        return self.record_and_fail(
                            req,
                            combo,
                            target,
                            dctx.fail_ctx_code(
                                &err,
                                Some(connect_and_send_ms),
                                Some(ttft_ms),
                                err.http_status(),
                            ),
                        );
                    }
                }
            }
            openproxy_types::TargetFormat::Atomesus => {
                let text = response_body_raw
                    .get("choices")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("message"))
                    .and_then(|m| m.get("content"))
                    .and_then(|s| s.as_str())
                    .or_else(|| response_body_raw.get("content").and_then(|s| s.as_str()))
                    .unwrap_or("");
                OpenAIResponse {
                    id: format!("chatcmpl_{}", uuid::Uuid::new_v4()),
                    object: "chat.completion".to_string(),
                    created: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs()),
                    model: req.openai_request.model.clone(),
                    choices: vec![openproxy_types::OpenAIChoice {
                        index: 0,
                        message: openproxy_types::OpenAIMessage {
                            role: "assistant".to_string(),
                            content: Some(serde_json::Value::String(text.to_string())),
                            name: None,
                            tool_call_id: None,
                            tool_calls: None,
                            extra: Default::default(),
                        },
                        finish_reason: Some("stop".to_string()),
                    }],
                    usage: None,
                }
            }
        };

        // Think-tag extraction: some providers (DeepSeek, Qwen, vLLM)
        // send reasoning inside `<think>...</think>` blocks in the
        // `content` field. Extract them into `reasoning_content` so
        // clients that parse think tags don't duplicate the reasoning,
        // and clients that don't parse tags don't show raw tags.
        let openai_response = extract_think_from_response(openai_response);

        // Bug fix: detect "empty response" — upstream returned 200 but
        // with content=null, finish_reason=null, no tool_calls, and no
        // reasoning. This is a provider bug (the model generated nothing
        // useful) and should be treated as an error so the pipeline
        // retries the next target instead of silently returning an
        // empty response to the client.
        let is_empty_response = openai_response.choices.first().is_some_and(|c| {
            let msg = &c.message;
            let content_empty = msg
                .content
                .as_ref()
                .is_none_or(|v| v.as_str().is_none_or(str::is_empty));
            let no_tool_calls = msg.tool_calls.as_ref().is_none_or(std::vec::Vec::is_empty);
            let no_reasoning = !msg.extra.contains_key("reasoning_content");
            let no_finish = c
                .finish_reason
                .as_ref()
                .is_none_or(|f| f == "null" || f.is_empty());
            content_empty && no_tool_calls && no_reasoning && no_finish
        });
        if is_empty_response {
            let err = CoreError::UpstreamConnection(
                "upstream returned 200 but response is empty (content=null, finish_reason=null, no tool_calls, no reasoning) — treating as error for retry".to_string(),
            );
            return self.record_and_fail(
                req,
                combo,
                target,
                dctx.fail_ctx_code(&err, Some(connect_and_send_ms), Some(ttft_ms), 502),
            );
        }

        let prompt_tokens = openai_response.usage.as_ref().map(|u| u.prompt_tokens);
        let completion_tokens = openai_response.usage.as_ref().map(|u| u.completion_tokens);
        let cached_tokens = openai_response
            .usage
            .as_ref()
            .and_then(|u| u.prompt_tokens_details.as_ref())
            .and_then(|d| d.cached_tokens);

        // Record the successful attempt and return.
        let total_ms_now = started.elapsed().as_millis() as u64;
        // C2 fix: redact sensitive headers (authorization,
        // cookie, x-api-key, etc.) before persisting them
        // to the `usage.request_headers` column. The chat
        // handler already redacts at the entry point, but
        // `dispatch_upstream` builds its own map from the
        // OpenAI provider's request headers and we have to
        // apply the same scrubbing here for code paths
        // that don't go through `chat.rs`.
        let request_headers_btm: std::collections::BTreeMap<String, String> =
            crate::redact::redact_btreemap_sensitive(
                headers
                    .iter()
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect(),
            );
        let usage_tuple =
            match crate::usage_tracker::UsageRecordBuilder::new(&self.tracker, req, combo, target)
                .proxy_url(req_proxy_url)
                .proxy_status(req_proxy_status)
                .model_opt(Some(model))
                .err_opt(None)
                .connect_ms_opt(Some(connect_and_send_ms))
                .ttft_ms_opt(Some(ttft_ms))
                .total_ms(total_ms_now)
                .status_code(status_code)
                .attempt(attempt)
                .race_size(race_size)
                .trace_id(trace_id.clone())
                .prompt_tokens_opt(prompt_tokens)
                .completion_tokens_opt(completion_tokens)
                .cached_tokens(cached_tokens)
                .response_body_json(Some(response_body_value))
                .request_headers(Some(request_headers_btm))
                .response_headers(response_headers)
                .is_streaming(false)
                .stream_complete(true)
                .stop_reason(None)
                .record()
            {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(error = %e, "UsageRecordBuilder failed; non-fatal");
                    None
                }
            };

        PipelineResult {
            status_code,
            error: None,
            final_response: Some(openai_response),
            attempts: attempt,
            usage_tuple,
        }
    }

    pub(crate) fn fail_stream_client_disconnected(
        &self,
        fctx: StreamFailureContext<'_>,
    ) -> PipelineResult {
        let StreamFailureContext {
            req,
            combo,
            target,
            attempt,
            race_size,
            started,
            model,
            connect_ms,
            ttft_ms: _,
            trace_id,
            acc,
            chunk_id,
            created,
            model_name,
            proxy_url,
            proxy_status,
        } = fctx;
        let dctx = DispatchContext {
            attempt,
            race_size,
            started,
            model,
            proxy_url,
            proxy_status,
        };

        let has_partial_content = acc.as_ref().is_some_and(|a| !a.is_empty());
        if let Some(ref a) = acc
            && let Some((code, message)) = a.extract_upstream_error_from_raw()
        {
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
                is_proxy_rotated: false,
            };
            let acc_ref: Option<&crate::sse_accumulator::ResponseAccumulator> = match acc {
                Some(a) => {
                    a.mark_partial();
                    Some(&*a)
                }
                None => None,
            };
            let fail_ctx = dctx.fail_ctx_code(&err, Some(connect_ms), None, code);
            return self.record_and_fail_with_trace_id_and_partial(crate::PartialFailureParams {
                req,
                combo,
                target,
                ctx: fail_ctx,
                trace_id,
                acc: acc_ref,
                chunk_id: Some(chunk_id),
                created,
                model_name,
            });
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
                "stream interrupted — client disconnected after receiving partial content".into(),
            )
        } else {
            CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected)
        };
        let fail_ctx = dctx.fail_ctx_code(&err, Some(connect_ms), None, 499);
        self.record_and_fail_with_trace_id_and_partial(crate::PartialFailureParams {
            req,
            combo,
            target,
            ctx: fail_ctx,
            trace_id,
            acc: acc_ref,
            chunk_id: Some(chunk_id),
            created,
            model_name,
        })
    }

    pub(crate) fn fail_on_sink_send_error(
        &self,
        e: crate::race_sink::StreamSinkError,
        fctx: StreamFailureContext<'_>,
    ) -> PipelineResult {
        let StreamFailureContext {
            req,
            combo,
            target,
            attempt,
            race_size,
            started,
            model,
            connect_ms,
            ttft_ms,
            trace_id,
            acc,
            chunk_id,
            created,
            model_name,
            proxy_url,
            proxy_status,
        } = fctx;
        let dctx = DispatchContext {
            attempt,
            race_size,
            started,
            model,
            proxy_url,
            proxy_status,
        };

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
                if let Some(ref a) = acc
                    && let Some((code, message)) = a.extract_upstream_error_from_raw()
                {
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
                            is_proxy_rotated: false,
                        };
                        let acc_ref: Option<&crate::sse_accumulator::ResponseAccumulator> =
                            match acc {
                                Some(a) => {
                                    a.mark_partial();
                                    Some(&*a)
                                }
                                None => None,
                            };
                        let fail_ctx = dctx.fail_ctx_code(&err, Some(connect_ms), None, code);
                        self.record_and_fail_with_trace_id_and_partial(
                            crate::PartialFailureParams {
                                req,
                                combo,
                                target,
                                ctx: fail_ctx,
                                trace_id,
                                acc: acc_ref,
                                chunk_id: Some(chunk_id),
                                created,
                                model_name,
                            },
                        )
                    };
                }
                let is_watchdog_fired = watchdog_fired.is_some();
                tracing::warn!(
                    combo_id = combo.id.0,
                    target_id = target.id.0,
                    provider = %target.provider_id,
                    model = %model.model_id.as_str(),
                    elapsed_ms = elapsed,
                    connect_ms = connect_ms,
                    ttft_ms = ?ttft_ms,
                    watchdog_fired = is_watchdog_fired,
                    "sink send failed: Closed — client/proxy disconnected \
                     (elapsed={}ms, connect={}ms, ttft={:?}, watchdog_fired={})",
                    elapsed, connect_ms, ttft_ms, is_watchdog_fired
                );
                CoreError::UpstreamConnection(format!(
                    "client disconnected (elapsed={elapsed}ms, connect={connect_ms}ms, ttft={ttft_ms:?}) — \
                     likely proxy idle timeout or client HTTP library timeout"
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
        let fail_ctx = dctx.fail_ctx_code(&err, Some(connect_ms), None, err.http_status());
        self.record_and_fail_with_trace_id_and_partial(crate::PartialFailureParams {
            req,
            combo,
            target,
            ctx: fail_ctx,
            trace_id,
            acc: acc_ref,
            chunk_id: Some(chunk_id),
            created,
            model_name,
        })
    }

    // ---------------------------------------------------------------------
    // Streaming upstream dispatch
    // ---------------------------------------------------------------------

    /// Streaming variant of dispatch_upstream. Reads SSE lines from
    /// the upstream response and forwards each translated chunk through
    /// the stream_sink channel in real-time.
    pub(crate) async fn dispatch_upstream_streaming(
        &self,
        params: StreamDispatchParams<'_>,
    ) -> PipelineResult {
        let StreamDispatchParams {
            target,
            combo,
            req,
            model,
            target_format,
            resolved_timeouts,
            started,
            attempt,
            race_size,
            trace_id,
            upstream_request,
        } = params;
        let dctx = DispatchContext {
            attempt,
            race_size,
            started,
            model,
            proxy_url: upstream_request.proxy.clone(),
            proxy_status: upstream_request.proxy_status.clone(),
        };

        let Some(sink) = req.stream_sink.as_ref() else {
            return self.record_and_fail(
                req,
                combo,
                target,
                dctx.fail_ctx_code(
                    &CoreError::Internal(
                        "dispatch_upstream_streaming called without stream_sink".into(),
                    ),
                    None,
                    None,
                    500,
                ),
            );
        };

        // Cancellation: the `client_disconnected` watch is the
        // operator's signal that the client has gone away. The
        // hyper-based upstream client accepts a `CancellationToken`;
        // we mirror the watch into a token via `from_watch`. The
        // token is consulted by the client at every phase boundary
        // (DNS, dial, TLS, write, headers, body chunk, total) AND
        // inside the `UpstreamBodyStream::next_chunk` between
        // frames — so the body loop below does NOT need its own
        // per-chunk cancel watch for the upstream-side cancellation
        // to fire. The `client_disconnected` watch IS still consulted
        // in the body loop, but only to short-circuit the
        // post-stream accounting (usage row, [DONE] sentinel) —
        // see the post-loop `is_client_disconnected` check.
        //
        // Pre-flight check: if the watch has ALREADY flipped to
        // `true` (e.g. the client disconnected while we were
        // building the request) we short-circuit to a structured
        // `Cancelled(openproxy_types::CancelReason::ClientDisconnected)` result without spinning up a hyper
        // request that we'd cancel 1 ms later.
        let send_start = Instant::now();
        let client_disconnected = *req.client_disconnected.borrow();
        if let Some(reason) = client_disconnected {
            let elapsed = send_start.elapsed().as_millis() as u64;
            tracing::warn!(
                combo_id = combo.id.0,
                target_id = target.id.0,
                provider = %target.provider_id,
                elapsed_ms = elapsed,
                "client disconnected before upstream streaming send; aborting attempt"
            );
            return self.record_and_fail(
                req,
                combo,
                target,
                dctx.fail_ctx_code(
                    &CoreError::Cancelled(reason),
                    Some(elapsed),
                    None,
                    CoreError::Cancelled(reason).http_status(),
                ),
            );
        }
        let cancel_token = if let Some(rc) = req.race_cancel.as_ref() {
            CancellationToken::from_watch_and_token(
                tokio::sync::watch::Receiver::clone(&req.client_disconnected),
                rc,
            )
        } else {
            CancellationToken::from_watch(tokio::sync::watch::Receiver::clone(
                &req.client_disconnected,
            ))
        };
        let req_proxy_url = upstream_request.proxy.clone();
        let req_proxy_status = upstream_request.proxy_status.clone();
        let result = self
            .config
            .upstream_client
            .call(
                upstream_request,
                openproxy_adapters::upstream::TimeoutProfile::Custom(
                    resolved_timeouts.as_resolved(),
                ),
                cancel_token,
            )
            .await;
        let connect_and_send_ms = send_start.elapsed().as_millis() as u64;

        // Map the `UpstreamError` taxonomy to the `CoreError` shape
        // the downstream code expects. Mirrors the non-streaming
        // path's mapping 1-to-1: a per-phase `UpstreamPhase` becomes
        // the `phase` label, the `Cancel` variant becomes a
        // structured `Cancelled(openproxy_types::CancelReason::ClientDisconnected)` result, and the rest
        // collapse to `UpstreamConnection`. The streaming path
        // doesn't have a "total" pre-migration mapping (it was
        // `phase: "total"` from legacy whole-request timeout),
        // so `Body` here maps to the same `"total"` label to keep
        // the dashboards consistent.
        let response_result: std::result::Result<
            openproxy_adapters::upstream::UpstreamResponse,
            UpstreamError,
        > = match result {
            Ok(r) => Ok(r),
            Err(UpstreamError::Cancel) => {
                tracing::warn!(
                    combo_id = combo.id.0,
                    target_id = target.id.0,
                    provider = %target.provider_id,
                    elapsed_ms = connect_and_send_ms,
                    "client cancelled during upstream streaming send; aborting attempt"
                );
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    dctx.fail_ctx_code(
                        &CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected),
                        Some(connect_and_send_ms),
                        None,
                        CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected)
                            .http_status(),
                    ),
                );
            }
            Err(UpstreamError::Timeout(phase)) => {
                let is_proxy_rotated = self
                    .check_and_trigger_proxy_rotation(
                        &target.provider_id,
                        target.account_id,
                        req.proxy_override.as_ref().map(|(pid, _)| pid.as_str()),
                        crate::upstream_dispatcher::ProxyRotationTrigger::ConnectError,
                        None,
                    )
                    .await;
                let phase_label = match phase {
                    openproxy_adapters::upstream::UpstreamPhase::Dns => "dns",
                    openproxy_adapters::upstream::UpstreamPhase::Dial => "dial",
                    openproxy_adapters::upstream::UpstreamPhase::Tls => "tls",
                    openproxy_adapters::upstream::UpstreamPhase::Write => "write",
                    openproxy_adapters::upstream::UpstreamPhase::Headers => "headers",
                    openproxy_adapters::upstream::UpstreamPhase::Body => "body",
                    openproxy_adapters::upstream::UpstreamPhase::Total => "total",
                };
                tracing::warn!(
                    combo_id = combo.id.0,
                    target_id = target.id.0,
                    provider = %target.provider_id,
                    phase = %phase,
                    elapsed_ms = connect_and_send_ms,
                    "upstream phase timed out; aborting streaming attempt"
                );
                let err = CoreError::UpstreamError {
                    status: 504,
                    provider: target.provider_id.to_string(),
                    model: model.model_id.as_str().to_string(),
                    body: format!(
                        "upstream phase `{phase_label}` timed out after {connect_and_send_ms}ms"
                    ),
                    is_proxy_rotated,
                };
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    dctx.fail_ctx_code(&err, Some(connect_and_send_ms), None, err.http_status()),
                );
            }
            Err(UpstreamError::Connection(msg) | UpstreamError::Tls(msg) |
UpstreamError::Http(msg) | UpstreamError::Decode(msg) |
UpstreamError::Invalid(msg)) => {
                let is_proxy_rotated = self
                    .check_and_trigger_proxy_rotation(
                        &target.provider_id,
                        target.account_id,
                        req.proxy_override.as_ref().map(|(pid, _)| pid.as_str()),
                        crate::upstream_dispatcher::ProxyRotationTrigger::ConnectError,
                        None,
                    )
                    .await;
                let err = CoreError::UpstreamError {
                    status: 502,
                    provider: target.provider_id.to_string(),
                    model: model.model_id.as_str().to_string(),
                    body: format!("upstream connection error: {msg}"),
                    is_proxy_rotated,
                };
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    dctx.fail_ctx_code(&err, Some(connect_and_send_ms), None, err.http_status()),
                );
            }
            Err(_) => {
                let is_proxy_rotated = self
                    .check_and_trigger_proxy_rotation(
                        &target.provider_id,
                        target.account_id,
                        req.proxy_override.as_ref().map(|(pid, _)| pid.as_str()),
                        crate::upstream_dispatcher::ProxyRotationTrigger::ConnectError,
                        None,
                    )
                    .await;
                let err = CoreError::UpstreamError {
                    status: 502,
                    provider: target.provider_id.to_string(),
                    model: model.model_id.as_str().to_string(),
                    body: "unknown upstream error".to_string(),
                    is_proxy_rotated,
                };
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    dctx.fail_ctx_code(&err, Some(connect_and_send_ms), None, err.http_status()),
                );
            }
        };

        // `response_result` is `Ok` here because every error arm
        // above already returned. The `match` is needed to satisfy
        // the borrow checker (we move out of the binding), but
        // we make the `Err` arm unreachable so the compiler is
        // happy.
        let response = match response_result {
            Ok(r) => r,
            Err(e) => unreachable!(
                "dispatch_upstream_streaming: response_result was expected to be Ok after error-mapping match; got {:?}",
                e
            ),
        };

        let status_code = response.status.as_u16();
        if !(200..300).contains(&status_code) {
            let mut is_proxy_rotated = self
                .check_and_trigger_proxy_rotation(
                    &target.provider_id,
                    target.account_id,
                    req.proxy_override.as_ref().map(|(pid, _)| pid.as_str()),
                    crate::upstream_dispatcher::ProxyRotationTrigger::Status(status_code),
                    None,
                )
                .await;
            // Error responses should not stall the pipeline. We give the upstream
            // 5 seconds to send the error body; if it stalls, we drop the body
            // and proceed with the error status code. This prevents "ghost" requests
            // stuck in `connecting` for 300s when an upstream hangs after sending headers.
            let body_str = match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                response.body.collect_all(),
            )
            .await
            {
                Ok(Ok(b)) => String::from_utf8_lossy(&b).to_string(),
                _ => String::new(),
            };
            // G2.3: surface `account_invalid` on 401/403 (mirrors the
            // non-streaming path's hook above). The streaming path
            // can hit this branch BEFORE any byte is streamed to the
            // client — the upstream rejects the auth on the request
            // headers, returns a non-2xx with a body, and we surface
            // it as `UpstreamError`. See the non-streaming hook for
            // the full rationale.
            if (status_code == 401 || status_code == 403)
                && let Some(aid) = target.account_id
            {
                let provider_id_str = target.provider_id.to_string();
                let model_id_str = model.model_id.as_str().to_string();
                let dedup_key = format!("account_invalid:{}", aid.0);
                let payload = serde_json::json!({
                    "code": "account_invalid",
                    "message": format!(
                        "Account {} on {} rejected by upstream (HTTP {})",
                        aid.0, provider_id_str, status_code,
                    ),
                    "provider_id": &provider_id_str,
                    "details": {
                        "account_id": aid.0,
                        "provider_id": &provider_id_str,
                        "model_id": &model_id_str,
                        "status_code": status_code,
                    },
                });
                let repo = Arc::clone(&self.tracker.repo);
                let provider_id_str_clone = provider_id_str.clone();
                tokio::task::spawn_blocking(move || {
                    let _ = repo.insert_and_broadcast_notification(
                        "system",
                        &payload,
                        Some(&dedup_key),
                        Some(&provider_id_str_clone),
                    );
                })
                .await
                .ok();
            }
            // NEW-2 fix: when the upstream returns 429 (or 408/503)
            // with a `Retry-After` header, surface the error as
            // `CoreError::RateLimited` so the per-target retry loop
            // honors the upstream-requested delay instead of using
            // the fixed exponential backoff. Mirrors the non-streaming
            // path's handling at line 3172.
            let retry_after_ms: Option<u64> = response
                .headers
                .get("retry-after")
                .or_else(|| response.headers.get("Retry-After"))
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after_ms);
            let is_rate_limited_status =
                status_code == 429 || status_code == 408 || status_code == 503;
            let err = if is_rate_limited_status {
                let retry_ms = retry_after_ms.unwrap_or(300_000);
                if !is_proxy_rotated {
                    is_proxy_rotated = self
                        .check_and_trigger_proxy_rotation(
                            &target.provider_id,
                            target.account_id,
                            req.proxy_override.as_ref().map(|(pid, _)| pid.as_str()),
                            crate::upstream_dispatcher::ProxyRotationTrigger::RateLimited,
                            Some(retry_ms),
                        )
                        .await;
                }
                CoreError::RateLimited {
                    provider: target.provider_id.to_string(),
                    retry_after_ms: retry_ms,
                    is_proxy_rotated,
                }
            } else {
                // Diagnostic: when MiniMax returns a 400 with error
                // code 2013 ("tool call and result not match" or
                // "tool call result does not follow tool call"), log
                // the full error body and the request's tool-related
                // metadata so we can diagnose the translation bug.
                // This is the most common MiniMax failure and the
                // error message alone doesn't tell us which
                // tool_use/tool_result pair is the problem.
                if status_code == 400 && body_str.contains("2013") {
                    tracing::warn!(
                        status_code = status_code,
                        provider = %target.provider_id,
                        model = %model.model_id.as_str(),
                        error_body = %body_str,
                        openai_request_messages_count = req.openai_request.messages.len(),
                        openai_request_tools_count = req.openai_request.tools.as_ref().map_or(0, std::vec::Vec::len),
                        "MiniMax 2013 error: tool_call/tool_result mismatch. \
                         Enable RUST_LOG=openproxy_core::translation=debug to see the \
                         translated Anthropic message structure."
                    );
                }
                CoreError::UpstreamError {
                    status: status_code,
                    provider: target.provider_id.to_string(),
                    model: model.model_id.as_str().to_string(),
                    body: body_str,
                    is_proxy_rotated,
                }
            };
            return self.record_and_fail(
                req,
                combo,
                target,
                dctx.fail_ctx_code(&err, Some(connect_and_send_ms), None, status_code),
            );
        }

        let chunk_id = format!("chatcmpl-{}", uuid::Uuid::new_v4());
        let created = chrono::Utc::now().timestamp() as u64;
        let model_name = model.model_id.as_str().to_string();

        // Emit `waiting_ttft` stage event: HTTP headers received,
        // body streaming next. This matches the non-streaming path's
        // stage sequence (started → connecting → waiting_ttft →
        // streaming → completed).
        openproxy_types::usage::publish_stage_event(openproxy_types::usage::StageEvent {
            request_id: req.request_id.to_string(),
            trace_id: trace_id.clone(),
            provider_id: None,
            upstream_model_id: None,
            stage: "waiting_ttft".into(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            connect_ms: Some(connect_and_send_ms),
            ttft_ms: None,
            status_code: Some(status_code),
            error: None,
            stop_reason: None,
            timestamp: None,
            endpoint_kind: None,
        });

        // The first SSE chunk emits the `streaming` stage event
        // (see the `if ttft_ms.is_none()` branch below) so we know
        // `ttft_ms` exactly at that moment. We deliberately do NOT
        // emit a `streaming` event here at the start of the loop
        // — the operator's "ttft" number is the time from socket
        // open to first body byte, and a separate "headers in"
        // event would imply we have a distinct timing for that,
        // which we don't. The `waiting_ttft` event we emitted a
        // few lines above already covers "headers received, body
        // streaming next".

        // Read the response as a byte stream, split into lines,
        // and process each SSE line.
        //
        // `UpstreamBodyStream` does NOT implement `futures::Stream`
        // (intentionally — see `upstream::response`); we iterate it
        // via `next_chunk().await` instead. The hyper-based stream
        // already consults the `CancellationToken` and the
        // per-chunk deadline between frames, so the loop's only
        // extra responsibility is to surface the `client_disconnected`
        // watch transition into the cancellation path: when the
        // watch flips, the body future is dropped (cancelling the
        // hyper body) and the loop exits cleanly. We do NOT
        // short-circuit by `None`-ing the chunk arm of the select
        // here — returning `UpstreamBodyStream::next_chunk`'s actual
        // result keeps the existing post-loop accounting
        // (usage row, [DONE] sentinel) running.
        let mut stream = response.body;
        // RAM optimization: 4096 bytes (was 8192). SSE lines are
        // typically <2 KB; 4 KB is enough for most chunks and halves
        // the per-stream buffer reservation. The buffer grows
        // dynamically via `reserve` below if a line exceeds it.
        let mut state = crate::streaming_state::StreamingState::new(true);

        let ctx = crate::streaming_state::StreamContext {
            req: &req,
            combo,
            target,
            model,
            target_format,
            sink,
            trace_id: &trace_id,
            chunk_id: &chunk_id,
            model_name: &model_name,
            started,
            attempt,
            race_size,
            created,
            connect_and_send_ms,
            resolved_timeouts,
            proxy_url: dctx.proxy_url.clone(),
            proxy_status: dctx.proxy_status.clone(),
        };

        match state.run_stream_loop(&ctx, self, &mut stream).await {
            Ok(crate::streaming_state::ChunkResult::Return(r)) => return *r,
            Ok(crate::streaming_state::ChunkResult::Break) => {}
            Err(e) => {
                // If the stream loop failed with a CoreError (e.g. I/O error reading body),
                // we treat it as an upstream error and fail.
                return self.record_and_fail_with_trace_id(
                    req.clone(),
                    combo,
                    target,
                    dctx.fail_ctx_code(&e, Some(connect_and_send_ms), state.ttft_ms, 502),
                    trace_id.clone(),
                );
            }
        }

        let client_disconnected = if state.done_sent {
            None
        } else {
            let mut rx = tokio::sync::watch::Receiver::clone(&req.client_disconnected);
            self.is_client_disconnected(&mut rx)
        };

        if let Some(_reason) = client_disconnected {
            tracing::warn!(
                combo_id = combo.id.0,
                target_id = target.id.0,
                provider = %target.provider_id,
                "client cancelled during SSE stream; aborting attempt"
            );
            return self.fail_stream_client_disconnected(StreamFailureContext {
                proxy_url: req_proxy_url.clone(),
                proxy_status: req_proxy_status.clone(),
                req: req.clone(),
                combo,
                target,
                attempt,
                race_size,
                started,
                model,
                connect_ms: connect_and_send_ms,
                ttft_ms: state.ttft_ms,
                trace_id: trace_id.clone(),
                acc: state.acc.as_mut(),
                chunk_id: &chunk_id,
                created,
                model_name: &model_name,
            });
        }

        let usage = state.usage;
        let mut acc = state.acc;
        let ttft_ms = state.ttft_ms;
        let stop_reason = state.stop_reason;
        let done_sent = state.done_sent;

        let total_ms = started.elapsed().as_millis() as u64;

        // Bug fix: detect "empty streaming response" — the stream
        // completed (done_sent or EOF) but the accumulator has no
        // content, no reasoning, no tool_calls. This happens with
        // providers like nvidia-nim/minimax-m3 (Anthropic format)
        // that return 200 + empty content + null finish_reason.
        // Treat as error so the pipeline retries the next target.
        // We only do this if stop_reason is None, because an empty
        // stream with finish_reason="length" (max_tokens=1 cut it off)
        // is perfectly valid.
        let is_empty_stream = acc.as_ref().is_some_and(super::sse_accumulator::ResponseAccumulator::is_empty) && stop_reason.is_none();
        if is_empty_stream {
            let err = CoreError::UpstreamConnection(
                "streaming response was empty (no content, no reasoning, no tool_calls) — treating as error for retry".to_string(),
            );
            let acc_ref: Option<&crate::sse_accumulator::ResponseAccumulator> = match &mut acc {
                Some(a) => {
                    a.mark_partial();
                    Some(&*a)
                }
                None => None,
            };
            return self.record_and_fail_with_trace_id_and_partial(crate::PartialFailureParams {
                req,
                combo,
                target,
                ctx: dctx.fail_ctx_code(&err, Some(connect_and_send_ms), None, 502),
                trace_id,
                acc: acc_ref,
                chunk_id: Some(&chunk_id),
                created,
                model_name: &model_name,
            });
        }

        // Record usage.
        // H5: streaming-success semantics. `is_streaming` is
        // always true here (we came from the streaming
        // dispatch). `stream_complete` mirrors the
        // post-loop [DONE] flag — `done_sent` is true iff the
        // upstream emitted the sentinel before its connection
        // closed.
        let prompt_tokens = usage.as_ref().map(|u| u.prompt_tokens);
        let completion_tokens = usage.as_ref().map(|u| u.completion_tokens);
        let cached_tokens = usage
            .as_ref()
            .and_then(|u| u.prompt_tokens_details.as_ref())
            .and_then(|d| d.cached_tokens);
        // G1 fix: assemble the persisted response body. The accumulator
        // is `Some(_)` only when `is_recording() == true` at function
        // entry, so when recording is OFF the only cost is a single
        // match on `acc.as_ref()`. The downstream `is_recording` gate
        // at `UsageRecordBuilder`
        // drops the body to `None` if recording flipped off mid-stream.
        let response_body_json: Option<serde_json::Value> = acc
            .as_ref()
            .map(|a| a.finish(&chunk_id, created, &model_name));
        let final_response = if matches!(
            req.stream_sink.as_ref(),
            Some(crate::race_sink::StreamSink::Discard)
        ) {
            response_body_json
                .as_ref()
                .and_then(|v| serde::Deserialize::deserialize(v).ok())
        } else {
            None
        };

        // G1 fix: save the request body for streaming requests too.
        // Previously this was `None` ("out of scope per G1 spec") so
        // the detail modal always showed "No request body recorded"
        // for all streaming rows.
        // Prefer the raw request body (preserves unknown fields the
        // typed `OpenAIRequest` struct would drop). Fall back to
        // re-serializing the typed struct when the raw body wasn't
        // captured (e.g., requests constructed internally without
        // going through the HTTP handler).
        let usage_tuple =
            match crate::usage_tracker::UsageRecordBuilder::new(&self.tracker, req, combo, target)
                .proxy_url(req_proxy_url)
                .proxy_status(req_proxy_status)
                .model_opt(Some(model))
                .err_opt(None)
                .connect_ms_opt(Some(connect_and_send_ms))
                .ttft_ms_opt(ttft_ms)
                .total_ms(total_ms)
                .status_code(status_code)
                .attempt(attempt)
                .race_size(race_size)
                .trace_id(trace_id.clone())
                .prompt_tokens_opt(prompt_tokens)
                .completion_tokens_opt(completion_tokens)
                .cached_tokens(cached_tokens)
                .response_body_json(response_body_json)
                .request_headers(None)
                .response_headers(None)
                .is_streaming(true)
                .stream_complete(done_sent)
                .stop_reason(stop_reason)
                .record()
            {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(error = %e, "UsageRecordBuilder failed; non-fatal");
                    None
                }
            };

        PipelineResult {
            status_code,
            error: None,
            // For non-streaming clients (StreamSink::Discard), return
            // the accumulated response so the chat handler can serialize
            // it as JSON. For streaming clients, the chunks were already
            // forwarded via the sink — return None (the chat handler
            // doesn't need the full response, it already sent the SSE).
            final_response,
            attempts: attempt,
            usage_tuple,
        }
    }
}
