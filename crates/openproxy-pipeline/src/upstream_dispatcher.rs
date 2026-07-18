use crate::timeouts::Timeouts;
use crate::translation::OpenAIResponse;
use crate::{FailureContext, PipelineRequest, PipelineResult, parse_retry_after_ms};
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
            proxy_url: None,
            proxy_status: None,
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
    pub(crate) started: Instant,
    pub(crate) model: &'a Model,
    pub(crate) connect_ms: u64,
    pub(crate) ttft_ms: Option<u64>,
    pub(crate) trace_id: String,
    pub(crate) proxy_url: Option<String>,
    pub(crate) proxy_status: Option<String>,
    pub(crate) acc: Option<&'a mut crate::sse_accumulator::ResponseAccumulator>,
    pub(crate) chunk_id: &'a str,
    pub(crate) created: u64,
    pub(crate) model_name: &'a str,
}

#[derive(Clone)]
pub struct UpstreamDispatcher {
    pub conn: Arc<parking_lot::Mutex<rusqlite::Connection>>,
    pub config: crate::PipelineConfig,
    pub compression_stats_cell:
        Arc<parking_lot::RwLock<Option<openproxy_compression::stats::CompressionStats>>>,
    pub tracker: crate::usage_tracker::UsageTracker,
    pub record_bodies_and_headers: Arc<std::sync::atomic::AtomicBool>,
}

pub(crate) enum ProxyRotationTrigger {
    Status(u16),
    ConnectError,
    RateLimited,
}

impl UpstreamDispatcher {
    pub fn new(
        conn: Arc<parking_lot::Mutex<rusqlite::Connection>>,
        config: crate::PipelineConfig,
        compression_stats_cell: Arc<
            parking_lot::RwLock<Option<openproxy_compression::stats::CompressionStats>>,
        >,
        tracker: crate::usage_tracker::UsageTracker,
        record_bodies_and_headers: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            conn,
            config,
            compression_stats_cell,
            tracker,
            record_bodies_and_headers,
        }
    }

    pub fn is_recording(&self) -> bool {
        self.record_bodies_and_headers
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) async fn check_and_trigger_proxy_rotation(
        &self,
        provider_id: &openproxy_types::ids::ProviderId,
        trigger: crate::upstream_dispatcher::ProxyRotationTrigger,
    ) -> bool {
        let conn_clone = self.conn.clone();
        let provider_id = provider_id.clone();
        let repo = self.tracker.repo.clone();
        tokio::task::spawn_blocking(move || {
            let provider = {
                let conn = conn_clone.lock();
                openproxy_db::providers::get(&conn, &provider_id).unwrap_or(None)
            };

            if let Some(provider) = provider
                && provider.use_proxies {
                    let should_rotate = match trigger {
                        crate::upstream_dispatcher::ProxyRotationTrigger::RateLimited => true,
                        crate::upstream_dispatcher::ProxyRotationTrigger::Status(sc) => {
                            let errors_list: Vec<&str> = provider
                                .proxy_rotation_errors
                                .split(',')
                                .map(|s| s.trim())
                                .collect();
                            errors_list.contains(&sc.to_string().as_str())
                        }
                        crate::upstream_dispatcher::ProxyRotationTrigger::ConnectError => {
                            let errors_list: Vec<&str> = provider
                                .proxy_rotation_errors
                                .split(',')
                                .map(|s| s.trim())
                                .collect();
                            errors_list.contains(&"connect_error")
                                || errors_list.contains(&"timeout")
                        }
                    };

                    if should_rotate && let Some(ref bad_proxy_id) = provider.current_proxy_id {
                        tracing::warn!(
                            provider = %provider_id,
                            proxy_id = %bad_proxy_id,
                            "proxy rotation triggered: marking proxy as dead and clearing binding"
                        );
                        let _ = repo.update_proxy_status(bad_proxy_id, "dead", None);

                        let conn = conn_clone.lock();
                        let _ = openproxy_db::providers::update_current_proxy(
                            &conn,
                            &provider_id,
                            None,
                        );
                        return true;
                    }
                }
            false
        })
        .await
        .unwrap()
    }

    pub(crate) fn is_client_disconnected(&self, rx: &mut watch::Receiver<bool>) -> bool {
        *rx.borrow_and_update()
    }

    pub(crate) fn record_and_fail(
        &self,
        req: PipelineRequest,
        combo: &Combo,
        target: &ComboTarget,
        ctx: FailureContext<'_>,
    ) -> PipelineResult {
        self.record_and_fail_with_trace_id(
            req.clone(),
            combo,
            target,
            ctx,
            req.trace_id.to_string(),
        )
    }

    pub(crate) fn record_and_fail_with_trace_id(
        &self,
        req: PipelineRequest,
        combo: &Combo,
        target: &ComboTarget,
        ctx: FailureContext<'_>,
        trace_id: String,
    ) -> PipelineResult {
        self.tracker.record_and_fail_with_trace_id_and_partial(
            req, combo, target, ctx, trace_id, None, None, 0, "",
        )
    }

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
        self.tracker.record_and_fail_with_trace_id_and_partial(
            req, combo, target, ctx, trace_id, acc, chunk_id, created, model_name,
        )
    }

    // ponytail: [Demasiados argumentos] -> [Refactorizar a struct en el futuro]
    pub(crate) async fn dispatch_upstream(
        &self,
        target: &ComboTarget,
        combo: &Combo,
        req: PipelineRequest,
        model: &Model,
        target_format: openproxy_types::TargetFormat,
        url: &str,
        headers: &[(String, String)],
        body_bytes: bytes::Bytes,
        resolved_timeouts: &Timeouts,
        started: Instant,
        attempt: u8,
        race_size: u8,
        trace_id: String,
    ) -> PipelineResult {
        let dctx = DispatchContext {
            attempt,
            race_size,
            started,
            model,
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
        let proxy_result = {
            let repo = self.tracker.repo.clone();
            let provider_id = target.provider_id.clone();
            tokio::task::spawn_blocking(move || repo.get_or_assign_provider_proxy(&provider_id))
                .await
                .unwrap()
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
                let repo = self.tracker.repo.clone();
                let u = url.clone();
                tokio::task::spawn_blocking(move || repo.get_proxy_status_by_url(&u))
                    .await
                    .unwrap_or(None)
            }
            None => None,
        };
        upstream_request.proxy_status = proxy_status.clone();
        tracing::info!(
            proxy_used = ?upstream_request.proxy,
            proxy_status = %proxy_status.as_ref().unwrap_or(&"none".to_string()),
            "assigned proxy for upstream request"
        );

        // is_streaming is always true because we force stream=true
        // to the upstream (see comment above). The body-chunk gap
        // timeout (idle_chunk_ms) applies normally — but only AFTER
        // the first chunk arrives (the initial deadline is
        // total_deadline, not start + body_chunk_ms).
        upstream_request.is_streaming = true;
        // Caller-supplied headers (auth, content-type overrides from
        // the adapter, etc.) — `post_json` already sets
        // `Content-Type: application/json`, so `insert` overwrites if
        // a caller header collides (matches the UpstreamClient chain's
        // behavior with `.header(k, v)` which appends; we choose
        // overwrite for determinism — the adapter layer is
        // responsible for not setting conflicting headers).
        for (k, v) in headers {
            // HeaderMap's insert() requires HeaderName/HeaderValue;
            // parse the strings. Skip headers that fail to parse —
            // matches the previous `.header(k.as_str(), v.as_str())`
            // which also silently dropped invalid values.
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::from_bytes(k.as_bytes()),
                http::HeaderValue::from_str(v),
            ) {
                upstream_request.headers.insert(name, value);
            }
        }

        // ALWAYS use the streaming path to the upstream. This
        // simplifies the code (one path instead of two) and fixes
        // the timeout issues with non-streaming requests:
        // - TTFT (first token) is properly measured for both modes
        // - idle_chunk_ms only applies after the first token arrives
        // - The upstream LLM starts generating immediately instead
        //   of waiting for the full response before sending
        // - Cancel propagation is faster (can cancel mid-generation)
        //
        // For non-streaming clients (stream: false), the stream_sink
        // is a Direct channel that the chat handler reads from. The
        // pipeline sends SSE chunks; the chat handler accumulates
        // them and returns the full JSON when the stream completes.
        // The `is_streaming` flag on the UpstreamRequest is set
        // based on the client's preference, but the upstream call
        // always uses stream=true (set in the translation layer).
        if let Some(sink) = &req.stream_sink {
            return self
                .dispatch_upstream_streaming(
                    target,
                    combo,
                    req.clone(),
                    model,
                    target_format,
                    sink,
                    resolved_timeouts,
                    started,
                    attempt,
                    race_size,
                    trace_id,
                    upstream_request,
                )
                .await;
        }

        // Fallback: no stream_sink (shouldn't happen in production —
        // the chat handler always provides one). Uses the old
        // non-streaming path as a safety net.
        // building the request) we short-circuit to a structured
        // `ClientDisconnected` result. The pre-flight is the only
        // place we map `UpstreamError::Cancel` → `ClientDisconnected`
        // — see below for the rationale.
        let send_start = Instant::now();
        if *req.client_disconnected.borrow() {
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
                    &CoreError::ClientDisconnected,
                    Some(elapsed),
                    None,
                    CoreError::ClientDisconnected.http_status(),
                ),
            );
        }
        let cancel_token = CancellationToken::from_watch(req.client_disconnected.clone());
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
                        &CoreError::ClientDisconnected,
                        Some(connect_and_send_ms),
                        None,
                        CoreError::ClientDisconnected.http_status(),
                    ),
                );
            }
            Err(UpstreamError::Timeout(phase)) => {
                self.check_and_trigger_proxy_rotation(
                    &target.provider_id,
                    crate::upstream_dispatcher::ProxyRotationTrigger::ConnectError,
                )
                .await;
                // Bug fix: attribute the timeout to the CORRECT phase
                // instead of collapsing DNS/Dial/TLS/Write/Headers all
                // into "connect". The user configures per-phase budgets
                // (connect_ms, request_send_ms, ttft_ms) and the error
                // message must reflect which budget actually fired so
                // they can tune the right knob. The old mapping (all
                // → "connect") was a leftover from the pre-migration
                // legacy UpstreamClient path that couldn't separate phases.
                // Include the config field name so the operator
                // knows which timeout to adjust in the dashboard.
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
                let err = CoreError::UpstreamTimeout {
                    phase: format!("{} (config: {})", phase_label, config_hint),
                    ms: connect_and_send_ms,
                };
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    dctx.fail_ctx_code(&err, Some(connect_and_send_ms), None, err.http_status()),
                );
            }
            Err(UpstreamError::Connection(msg))
            | Err(UpstreamError::Tls(msg))
            | Err(UpstreamError::Http(msg))
            | Err(UpstreamError::Decode(msg))
            | Err(UpstreamError::Invalid(msg)) => {
                self.check_and_trigger_proxy_rotation(
                    &target.provider_id,
                    crate::upstream_dispatcher::ProxyRotationTrigger::ConnectError,
                )
                .await;
                let err = CoreError::UpstreamConnection(msg);
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    dctx.fail_ctx_code(&err, Some(connect_and_send_ms), None, err.http_status()),
                );
            }
            Err(_) => {
                self.check_and_trigger_proxy_rotation(
                    &target.provider_id,
                    crate::upstream_dispatcher::ProxyRotationTrigger::ConnectError,
                )
                .await;
                let err = CoreError::UpstreamConnection("unknown upstream error".to_string());
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
            let snapshot = self.compression_stats_cell.read().clone();
            openproxy_types::usage::publish_stage_event(openproxy_types::usage::StageEvent {
                request_id: req.request_id.to_string(),
                trace_id: trace_id.to_string(),
                provider_id: target.provider_id.to_string(),
                upstream_model_id: model.model_id.as_str().to_string(),
                stage: stage.into(),
                elapsed_ms: started.elapsed().as_millis() as u64,
                connect_ms: Some(connect_and_send_ms),
                ttft_ms: None,
                status_code: status,
                error: err,
                stop_reason: None,
                compression_savings_pct: snapshot.as_ref().and_then(|s| s.savings_pct_opt()),
                compression_techniques: snapshot.as_ref().and_then(|s| s.techniques_csv()),
                timestamp: String::new(),
                endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
            });
        };

        // Unwrap the `Ok` arm. The match above has already handled
        // every `Err` variant with an early `return` (or fell
        // through to `Ok`). This is just the `let response = match
        // { Ok(r) => r, Err(_) => unreachable!() }` of the original
        // code, expressed with `into_result` semantics.
        let response = match response_result {
            Ok(r) => r,
            Err(_) => unreachable!("error variants are handled above with early return"),
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
                        &CoreError::ClientDisconnected,
                        Some(connect_and_send_ms),
                        Some(ttft_ms),
                        CoreError::ClientDisconnected.http_status(),
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
                    crate::upstream_dispatcher::ProxyRotationTrigger::ConnectError,
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
                    crate::upstream_dispatcher::ProxyRotationTrigger::ConnectError,
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
                    crate::upstream_dispatcher::ProxyRotationTrigger::Status(status_code),
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
            if let Some(retry_ms) = retry_after_ms.filter(|_| is_rate_limited_status) {
                if !is_proxy_rotated {
                    is_proxy_rotated = self
                        .check_and_trigger_proxy_rotation(
                            &target.provider_id,
                            crate::upstream_dispatcher::ProxyRotationTrigger::RateLimited,
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
                    dctx.fail_ctx_code(&err, Some(connect_and_send_ms), Some(ttft_ms), status_code),
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
                let repo = self.tracker.repo.clone();
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
                .unwrap();
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
        let model_name = model.model_id.as_str().to_string();
        let streaming_snapshot = self.compression_stats_cell.read().clone();
        // Emit `waiting_ttft` before `streaming` for stage sequence
        // consistency with the non-streaming path. The streaming path
        // previously skipped this, but now that non-streaming clients
        // also go through the streaming path, we need it for the
        // stage sequence test to pass.
        openproxy_types::usage::publish_stage_event(openproxy_types::usage::StageEvent {
            request_id: req.request_id.to_string(),
            trace_id: trace_id.to_string(),
            provider_id: target.provider_id.to_string(),
            upstream_model_id: model_name.clone(),
            stage: "waiting_ttft".into(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            connect_ms: Some(connect_and_send_ms),
            ttft_ms: None,
            status_code,
            error: None,
            stop_reason: None,
            compression_savings_pct: None,
            compression_techniques: None,
            timestamp: chrono::Utc::now().to_rfc3339(),
            endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
        });
        openproxy_types::usage::publish_stage_event(openproxy_types::usage::StageEvent {
            request_id: req.request_id.to_string(),
            trace_id: trace_id.to_string(),
            provider_id: target.provider_id.to_string(),
            upstream_model_id: model_name,
            stage: "streaming".into(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            connect_ms: Some(connect_and_send_ms),
            ttft_ms: Some(ttft_ms),
            status_code,
            error: None,
            stop_reason: None,
            compression_savings_pct: streaming_snapshot
                .as_ref()
                .and_then(|s| s.savings_pct_opt()),
            compression_techniques: streaming_snapshot.as_ref().and_then(|s| s.techniques_csv()),
            timestamp: String::new(),
            endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
        });

        // 2xx: parse into the native wire format, then translate to
        // OpenAIResponse if needed.
        let response_body_raw: serde_json::Value = match serde_json::from_slice(&body_bytes) {
            Ok(v) => v,
            Err(e) => {
                let err = CoreError::Parse(format!("upstream json: {e}"));
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
                match serde_json::from_value::<OpenAIResponse>(response_body_raw.clone()) {
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
                    match serde_json::from_value(response_body_raw) {
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
                let gemini_resp: crate::translation::GeminiResponse =
                    match serde_json::from_value(response_body_raw) {
                        Ok(r) => r,
                        Err(e) => {
                            let err = CoreError::Parse(format!("parse gemini response: {e}"));
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
                crate::translation::gemini_to_openai(&gemini_resp)
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
                .is_none_or(|v| v.as_str().is_none_or(|s| s.is_empty()));
            let no_tool_calls = msg.tool_calls.as_ref().is_none_or(|t| t.is_empty());
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
            crate::redact::redact_btreemap_sensitive(headers.iter().cloned().collect());
        let usage_tuple = match crate::usage_tracker::UsageRecordBuilder::new(
            &self.tracker,
            req.clone(),
            combo,
            target,
        )
        .proxy_url(req_proxy_url.clone())
        .proxy_status(req_proxy_status.clone())
        .model_opt(Some(model))
        .err_opt(None)
        .connect_ms_opt(Some(connect_and_send_ms))
        .ttft_ms_opt(Some(ttft_ms))
        .total_ms(total_ms_now)
        .status_code(status_code)
        .attempt(attempt)
        .race_size(race_size)
        .trace_id(trace_id)
        .prompt_tokens_opt(prompt_tokens)
        .completion_tokens_opt(completion_tokens)
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
            let mut fail_ctx = dctx.fail_ctx_code(&err, Some(connect_ms), None, code);
            fail_ctx.proxy_url = proxy_url.clone();
            fail_ctx.proxy_status = proxy_status.clone();
            return self.record_and_fail_with_trace_id_and_partial(
                req,
                combo,
                target,
                fail_ctx,
                trace_id,
                acc_ref,
                Some(chunk_id),
                created,
                model_name,
            );
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
            CoreError::ClientDisconnected
        };
        let mut fail_ctx = dctx.fail_ctx_code(&err, Some(connect_ms), None, 499);
        fail_ctx.proxy_url = proxy_url;
        fail_ctx.proxy_status = proxy_status;
        self.record_and_fail_with_trace_id_and_partial(
            req,
            combo,
            target,
            fail_ctx,
            trace_id,
            acc_ref,
            Some(chunk_id),
            created,
            model_name,
        )
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
                        let mut fail_ctx = dctx.fail_ctx_code(&err, Some(connect_ms), None, code);
                        fail_ctx.proxy_url = proxy_url.clone();
                        fail_ctx.proxy_status = proxy_status.clone();
                        self.record_and_fail_with_trace_id_and_partial(
                            req,
                            combo,
                            target,
                            fail_ctx,
                            trace_id,
                            acc_ref,
                            Some(chunk_id),
                            created,
                            model_name,
                        )
                    };
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
        let mut fail_ctx = dctx.fail_ctx_code(&err, Some(connect_ms), None, err.http_status());
        fail_ctx.proxy_url = proxy_url;
        fail_ctx.proxy_status = proxy_status;
        self.record_and_fail_with_trace_id_and_partial(
            req,
            combo,
            target,
            fail_ctx,
            trace_id,
            acc_ref,
            Some(chunk_id),
            created,
            model_name,
        )
    }

    // ---------------------------------------------------------------------
    // Streaming upstream dispatch
    // ---------------------------------------------------------------------

    /// Streaming variant of dispatch_upstream. Reads SSE lines from
    /// the upstream response and forwards each translated chunk through
    /// the stream_sink channel in real-time.
    // ponytail: [Demasiados argumentos] -> [Refactorizar a struct en el futuro]
    pub(crate) async fn dispatch_upstream_streaming(
        &self,
        target: &ComboTarget,
        combo: &Combo,
        req: PipelineRequest,
        model: &Model,
        target_format: openproxy_types::TargetFormat,
        sink: &crate::race_sink::StreamSink,
        resolved_timeouts: &Timeouts,
        started: Instant,
        attempt: u8,
        race_size: u8,
        trace_id: String,
        upstream_request: UpstreamRequest,
    ) -> PipelineResult {
        let dctx = DispatchContext {
            attempt,
            race_size,
            started,
            model,
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
        // `ClientDisconnected` result without spinning up a hyper
        // request that we'd cancel 1 ms later.
        let send_start = Instant::now();
        if *req.client_disconnected.borrow() {
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
                    &CoreError::ClientDisconnected,
                    Some(elapsed),
                    None,
                    CoreError::ClientDisconnected.http_status(),
                ),
            );
        }
        let cancel_token = if let Some(rc) = req.race_cancel.as_ref() {
            CancellationToken::from_watch_and_token(req.client_disconnected.clone(), rc.clone())
        } else {
            CancellationToken::from_watch(req.client_disconnected.clone())
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
        // structured `ClientDisconnected` result, and the rest
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
                        &CoreError::ClientDisconnected,
                        Some(connect_and_send_ms),
                        None,
                        CoreError::ClientDisconnected.http_status(),
                    ),
                );
            }
            Err(UpstreamError::Timeout(phase)) => {
                self.check_and_trigger_proxy_rotation(
                    &target.provider_id,
                    crate::upstream_dispatcher::ProxyRotationTrigger::ConnectError,
                )
                .await;
                // Bug fix (PR #33): attribute the timeout to the
                // CORRECT phase instead of collapsing all into
                // "connect". Mirrors the non-streaming path's fix.
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
                let err = CoreError::UpstreamTimeout {
                    phase: phase_label.to_string(),
                    ms: connect_and_send_ms,
                };
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    dctx.fail_ctx_code(&err, Some(connect_and_send_ms), None, err.http_status()),
                );
            }
            Err(UpstreamError::Connection(msg))
            | Err(UpstreamError::Tls(msg))
            | Err(UpstreamError::Http(msg))
            | Err(UpstreamError::Decode(msg))
            | Err(UpstreamError::Invalid(msg)) => {
                self.check_and_trigger_proxy_rotation(
                    &target.provider_id,
                    crate::upstream_dispatcher::ProxyRotationTrigger::ConnectError,
                )
                .await;
                let err = CoreError::UpstreamConnection(msg);
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    dctx.fail_ctx_code(&err, Some(connect_and_send_ms), None, err.http_status()),
                );
            }
            Err(_) => {
                self.check_and_trigger_proxy_rotation(
                    &target.provider_id,
                    crate::upstream_dispatcher::ProxyRotationTrigger::ConnectError,
                )
                .await;
                let err = CoreError::UpstreamConnection("unknown upstream error".to_string());
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
                    crate::upstream_dispatcher::ProxyRotationTrigger::Status(status_code),
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
                let repo = self.tracker.repo.clone();
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
                .unwrap();
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
            let err = if let Some(retry_ms) = retry_after_ms.filter(|_| is_rate_limited_status) {
                if !is_proxy_rotated {
                    is_proxy_rotated = self
                        .check_and_trigger_proxy_rotation(
                            &target.provider_id,
                            crate::upstream_dispatcher::ProxyRotationTrigger::RateLimited,
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
                        openai_request_tools_count = req.openai_request.tools.as_ref().map(|t| t.len()).unwrap_or(0),
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
            trace_id: trace_id.to_string(),
            provider_id: target.provider_id.to_string(),
            upstream_model_id: model_name.clone(),
            stage: "waiting_ttft".into(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            connect_ms: Some(connect_and_send_ms),
            ttft_ms: None,
            status_code,
            error: None,
            stop_reason: None,
            compression_savings_pct: None,
            compression_techniques: None,
            timestamp: chrono::Utc::now().to_rfc3339(),
            endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
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
        };

        match state.run_stream_loop(&ctx, self, &mut stream).await {
            Ok(crate::streaming_state::ChunkResult::Return(r)) => return r,
            Ok(crate::streaming_state::ChunkResult::Break) => {}
            Err(e) => {
                // If the stream loop failed with a CoreError (e.g. I/O error reading body),
                // we treat it as an upstream error and fail.
                return self.record_and_fail_with_trace_id(
                    req.clone(),
                    combo,
                    target,
                    dctx.fail_ctx_code(&e, Some(connect_and_send_ms), state.ttft_ms, 502),
                    trace_id.to_string(),
                );
            }
        }

        let client_disconnected = if state.done_sent {
            false
        } else {
            let mut rx = req.client_disconnected.clone();
            self.is_client_disconnected(&mut rx)
        };

        if client_disconnected {
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
                trace_id: trace_id.to_string(),
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
        let is_empty_stream = acc.as_ref().is_some_and(|a| a.is_empty());
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
            return self.record_and_fail_with_trace_id_and_partial(
                req,
                combo,
                target,
                dctx.fail_ctx_code(&err, Some(connect_and_send_ms), None, 502),
                trace_id,
                acc_ref,
                Some(&chunk_id),
                created,
                &model_name,
            );
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
        // G1 fix: assemble the persisted response body. The accumulator
        // is `Some(_)` only when `is_recording() == true` at function
        // entry, so when recording is OFF the only cost is a single
        // match on `acc.as_ref()`. The downstream `is_recording` gate
        // at `UsageRecordBuilder`
        // drops the body to `None` if recording flipped off mid-stream.
        let response_body_json: Option<serde_json::Value> = acc
            .as_ref()
            .map(|a| a.finish(&chunk_id, created, &model_name));
        // G1 fix: save the request body for streaming requests too.
        // Previously this was `None` ("out of scope per G1 spec") so
        // the detail modal always showed "No request body recorded"
        // for all streaming rows.
        // Prefer the raw request body (preserves unknown fields the
        // typed `OpenAIRequest` struct would drop). Fall back to
        // re-serializing the typed struct when the raw body wasn't
        // captured (e.g., requests constructed internally without
        // going through the HTTP handler).
        let usage_tuple = match crate::usage_tracker::UsageRecordBuilder::new(
            &self.tracker,
            req.clone(),
            combo,
            target,
        )
        .proxy_url(req_proxy_url.clone())
        .proxy_status(req_proxy_status.clone())
        .model_opt(Some(model))
        .err_opt(None)
        .connect_ms_opt(Some(connect_and_send_ms))
        .ttft_ms_opt(ttft_ms)
        .total_ms(total_ms)
        .status_code(status_code)
        .attempt(attempt)
        .race_size(race_size)
        .trace_id(trace_id)
        .prompt_tokens_opt(prompt_tokens)
        .completion_tokens_opt(completion_tokens)
        .response_body_json(response_body_json.clone())
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
            final_response: if matches!(
                req.stream_sink.as_ref(),
                Some(crate::race_sink::StreamSink::Discard)
            ) {
                response_body_json
                    .as_ref()
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
            } else {
                None
            },
            attempts: attempt,
            usage_tuple,
        }
    }
}
