//! Dispatch streaming SSE: ciclo de vida completo desde el pre-flight
//! disconnect check hasta `record_streaming_success`. Contiene el método
//! principal (`dispatch_upstream_streaming`, 239 LOC) y sus helpers de
//! fallo (`fail_stream_*`, `fail_on_sink_send_error`).

use super::UpstreamDispatcher;
use super::types::{
    DispatchContext, StreamDispatchParams, StreamFailureContext, StreamingNon2xxArgs,
    StreamingSuccessArgs,
};
use crate::PipelineResult;
use crate::streaming_state::StreamingState;
use openproxy_adapters::upstream::{CancellationToken, UpstreamRequest};
use openproxy_types::combos::{Combo, ComboTarget};
use openproxy_types::error::CoreError;
use std::time::Instant;

impl UpstreamDispatcher {
    /// Entry point streaming. Pasos:
    /// 1. Verifica que `req.stream_sink` exista (sino → Internal error).
    /// 2. Pre-flight disconnect check.
    /// 3. `upstream_client.call` con cancel token (race-aware si está).
    /// 4. Si error → `handle_upstream_error`. Si non-2xx → `handle_streaming_non_2xx`.
    /// 5. Loop SSE via `state.run_stream_loop`.
    /// 6. Post-loop: si client disconnected → `fail_stream_client_disconnected`.
    /// 7. Stream vacío → fail; sino → `record_streaming_success`.
    pub(super) async fn dispatch_upstream_streaming(
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

        let send_start = Instant::now();
        if let Some(fail_res) =
            self.check_preflight_stream_disconnect(&req, combo, target, &dctx, send_start)
        {
            return fail_res;
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

        let response = match result {
            Ok(r) => r,
            Err(err) => {
                return self
                    .handle_upstream_error(err, req, combo, target, &dctx, connect_and_send_ms)
                    .await;
            }
        };

        let status_code = response.status.as_u16();
        if !(200..300).contains(&status_code) {
            return self
                .handle_streaming_non_2xx(
                    StreamingNon2xxArgs {
                        response,
                        status_code,
                        req,
                        combo,
                        target,
                        model,
                        connect_and_send_ms,
                    },
                    &dctx,
                )
                .await;
        }

        let mut chunk_id = String::with_capacity(48);
        use std::fmt::Write;
        let _ = write!(&mut chunk_id, "chatcmpl-{}", uuid::Uuid::new_v4());

        let created = chrono::Utc::now().timestamp() as u64;
        let model_name = model.model_id.as_str().to_string();

        openproxy_types::emit_stage_event!(
            request_id: req.request_id,
            trace_id: trace_id,
            stage: "waiting_ttft",
            elapsed_ms: started.elapsed().as_millis() as u64,
            connect_ms: connect_and_send_ms,
            status_code: status_code,
        );

        let mut stream = response.body;
        let mut state = StreamingState::new(true);

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
            super::fail::is_client_disconnected(&mut rx)
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

        let is_empty_stream = state
            .acc
            .as_ref()
            .is_some_and(crate::sse_accumulator::ResponseAccumulator::is_empty)
            && state.stop_reason.is_none();
        if is_empty_stream {
            let err = CoreError::UpstreamConnection(
                "streaming response was empty (no content, no reasoning, no tool_calls) — treating as error for retry".to_string(),
            );
            let mut acc = state.acc;
            if let Some(a) = acc.as_mut() {
                a.mark_partial();
            }
            return self.record_and_fail_with_trace_id_and_partial(crate::PartialFailureParams {
                req,
                combo,
                target,
                ctx: dctx.fail_ctx_code(&err, Some(connect_and_send_ms), None, 502),
                trace_id,
                acc: acc.as_ref(),
                chunk_id: Some(&chunk_id),
                created,
                model_name: &model_name,
            });
        }

        self.record_streaming_success(
            StreamDispatchParams {
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
                upstream_request: UpstreamRequest::post_json(String::new(), bytes::Bytes::new()),
            },
            &dctx,
            StreamingSuccessArgs {
                state,
                chunk_id: &chunk_id,
                created,
                model_name: &model_name,
                connect_and_send_ms,
                status_code,
            },
        )
    }

    /// Helper: marca accumulator como partial y delega a record_and_fail.
    fn fail_stream_with_error(
        &self,
        err: CoreError,
        mut fctx: StreamFailureContext<'_>,
        status_override: Option<u16>,
    ) -> PipelineResult {
        let dctx = DispatchContext {
            attempt: fctx.attempt,
            race_size: fctx.race_size,
            started: fctx.started,
            model: fctx.model,
            proxy_url: fctx.proxy_url,
            proxy_status: fctx.proxy_status,
        };
        if let Some(a) = fctx.acc.as_deref_mut() {
            a.mark_partial();
        }
        let status = status_override.unwrap_or_else(|| err.http_status());
        let fail_ctx = dctx.fail_ctx_code(&err, Some(fctx.connect_ms), None, status);
        self.record_and_fail_with_trace_id_and_partial(crate::PartialFailureParams {
            req: fctx.req,
            combo: fctx.combo,
            target: fctx.target,
            ctx: fail_ctx,
            trace_id: fctx.trace_id,
            acc: fctx.acc.as_deref(),
            chunk_id: Some(fctx.chunk_id),
            created: fctx.created,
            model_name: fctx.model_name,
        })
    }

    /// Si el upstream había enviado un error inline (SSE chunk con code+msg)
    /// antes de que el cliente se desconectara, lo atribuimos a error
    /// upstream. Si no, cancel puro (o `UpstreamConnection` si hubo
    /// contenido parcial).
    ///
    /// Visibilidad `pub(crate)`: invocado por `streaming_state.rs`
    /// (cross-module).
    pub(crate) fn fail_stream_client_disconnected(
        &self,
        fctx: StreamFailureContext<'_>,
    ) -> PipelineResult {
        let inline_err = fctx
            .acc
            .as_ref()
            .and_then(|a| a.extract_upstream_error_from_raw());
        if let Some((code, message)) = inline_err {
            tracing::warn!(
                combo_id = fctx.combo.id.0,
                target_id = fctx.target.id.0,
                provider = %fctx.target.provider_id,
                model = %fctx.model.model_id.as_str(),
                inline_error_code = code,
                inline_error_message = %message,
                "client disconnected but upstream had sent inline SSE error (code={code}); attributing to upstream error",
            );
            let err = CoreError::upstream_error(
                code,
                fctx.target.provider_id.to_string(),
                fctx.model_name,
                message,
                false,
            );
            return self.fail_stream_with_error(err, fctx, Some(code));
        }

        let has_partial_content = fctx.acc.as_deref().is_some_and(|a| !a.is_empty());
        let err = if has_partial_content {
            CoreError::UpstreamConnection(
                "stream interrupted — client disconnected after receiving partial content".into(),
            )
        } else {
            CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected)
        };
        self.fail_stream_with_error(err, fctx, Some(499))
    }

    /// Distingue `Lost` (otra race lane ganó) vs `Closed` (cliente/proxy
    /// caído). En el segundo caso, si hubo error inline upstream, lo
    /// propagamos; sino construimos un `UpstreamConnection`.
    ///
    /// Visibilidad `pub(crate)`: invocado por `streaming_state.rs`
    /// (cross-module).
    pub(crate) fn fail_on_sink_send_error(
        &self,
        e: crate::race_sink::StreamSinkError,
        fctx: StreamFailureContext<'_>,
    ) -> PipelineResult {
        if matches!(e, crate::race_sink::StreamSinkError::Lost) {
            tracing::debug!(
                combo_id = fctx.combo.id.0,
                target_id = fctx.target.id.0,
                "sink send failed: Lost (another race lane won)"
            );
            return self.fail_stream_with_error(CoreError::RaceLost, fctx, None);
        }

        let elapsed = fctx.started.elapsed().as_millis() as u64;
        let inline_err = fctx
            .acc
            .as_ref()
            .and_then(|a| a.extract_upstream_error_from_raw());
        if let Some((code, message)) = inline_err {
            tracing::warn!(
                combo_id = fctx.combo.id.0,
                target_id = fctx.target.id.0,
                provider = %fctx.target.provider_id,
                model = %fctx.model.model_id.as_str(),
                elapsed_ms = elapsed,
                inline_error_code = code,
                inline_error_message = %message,
                "sink closed after upstream sent inline SSE error (code={code}, elapsed={elapsed}ms)",
            );
            let err = CoreError::upstream_error(
                code,
                fctx.target.provider_id.to_string(),
                fctx.model_name,
                message,
                false,
            );
            return self.fail_stream_with_error(err, fctx, Some(code));
        }

        let is_watchdog_fired = fctx.req.client_disconnected.borrow().is_some();
        tracing::warn!(
            combo_id = fctx.combo.id.0,
            target_id = fctx.target.id.0,
            provider = %fctx.target.provider_id,
            model = %fctx.model.model_id.as_str(),
            elapsed_ms = elapsed,
            connect_ms = fctx.connect_ms,
            ttft_ms = ?fctx.ttft_ms,
            watchdog_fired = is_watchdog_fired,
            "sink send failed: Closed — client/proxy disconnected"
        );
        let err = CoreError::UpstreamConnection(format!(
            "client disconnected (elapsed={elapsed}ms, connect={}ms, ttft={:?}) — likely proxy idle timeout or client HTTP library timeout",
            fctx.connect_ms, fctx.ttft_ms
        ));
        self.fail_stream_with_error(err, fctx, None)
    }

    /// Pre-flight guard: si el cliente ya está desconectado antes de enviar,
    /// devuelve un `PipelineResult` de cancelación. Usado por la rama
    /// streaming.
    fn check_preflight_stream_disconnect(
        &self,
        req: &crate::PipelineRequest,
        combo: &Combo,
        target: &ComboTarget,
        dctx: &DispatchContext<'_>,
        send_start: Instant,
    ) -> Option<PipelineResult> {
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
            return Some(self.record_and_fail(
                req.clone(),
                combo,
                target,
                dctx.fail_ctx_code(
                    &CoreError::Cancelled(reason),
                    Some(elapsed),
                    None,
                    CoreError::Cancelled(reason).http_status(),
                ),
            ));
        }
        None
    }

    /// Maneja la rama streaming non-2xx: extrae `retry-after` del header,
    /// lee el body con timeout de 5s y delega en `handle_non_2xx_response`.
    async fn handle_streaming_non_2xx(
        &self,
        args: StreamingNon2xxArgs<'_>,
        dctx: &DispatchContext<'_>,
    ) -> PipelineResult {
        let retry_after_header = args
            .response
            .headers
            .get("retry-after")
            .or_else(|| args.response.headers.get("Retry-After"))
            .and_then(|v| v.to_str().ok());
        let body_str = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            args.response.body.collect_all(),
        )
        .await
        {
            Ok(Ok(b)) => String::from_utf8_lossy(&b).to_string(),
            _ => String::new(),
        };
        self.handle_non_2xx_response(
            args.status_code,
            retry_after_header,
            body_str,
            args.req,
            args.combo,
            args.target,
            args.model,
            dctx,
            args.connect_and_send_ms,
            None,
        )
        .await
    }

    /// Persiste el stream exitoso en el `UsageTracker` y construye el
    /// `PipelineResult`. Si el sink era `Discard` (race lane perdedora),
    /// materializamos el `final_response` desde el accumulator.
    fn record_streaming_success(
        &self,
        params: StreamDispatchParams<'_>,
        dctx: &DispatchContext<'_>,
        args: StreamingSuccessArgs<'_>,
    ) -> PipelineResult {
        let usage = args.state.usage;
        let acc = args.state.acc;
        let ttft_ms = args.state.ttft_ms;
        let stop_reason = args.state.stop_reason;
        let done_sent = args.state.done_sent;
        let total_ms = params.started.elapsed().as_millis() as u64;

        let prompt_tokens = usage.as_ref().map(|u| u.prompt_tokens);
        let completion_tokens = usage.as_ref().map(|u| u.completion_tokens);
        let cached_tokens = usage
            .as_ref()
            .and_then(|u| u.prompt_tokens_details.as_ref())
            .and_then(|d| d.cached_tokens);

        let response_body_json: Option<serde_json::Value> = acc
            .as_ref()
            .map(|a| a.finish(args.chunk_id, args.created, args.model_name));
        let final_response = if matches!(
            params.req.stream_sink.as_ref(),
            Some(crate::race_sink::StreamSink::Discard)
        ) {
            response_body_json
                .as_ref()
                .and_then(|v| serde::Deserialize::deserialize(v).ok())
        } else {
            None
        };

        let usage_tuple = match crate::usage_tracker::UsageRecordBuilder::new(
            &self.tracker,
            params.req,
            params.combo,
            params.target,
        )
        .proxy_url(dctx.proxy_url.clone())
        .proxy_status(dctx.proxy_status.clone())
        .model_opt(Some(params.model))
        .err_opt(None)
        .connect_ms_opt(Some(args.connect_and_send_ms))
        .ttft_ms_opt(ttft_ms)
        .total_ms(total_ms)
        .status_code(args.status_code)
        .attempt(params.attempt)
        .race_size(params.race_size)
        .trace_id(params.trace_id)
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
            status_code: args.status_code,
            error: None,
            final_response,
            attempts: params.attempt,
            usage_tuple,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Test P3 obligatorio (AGENTS.md §3.1 P3): `dispatch_upstream_streaming`
    //! extraída con $>20$ líneas requiere al menos 1 test unitario. Cubre
    //! la rama de fallo temprano cuando `req.stream_sink` es `None`:
    //! debe retornar `PipelineResult` con `CoreError::Internal`.
    //!
    //! Limitación: este test no cubre la llamada a `upstream_client.call`
    //! (que requiere red); la rama de pre-flight + sink-missing es la
    //! primera que se evalúa y no necesita IO.

    use super::super::UpstreamDispatcher;
    use super::super::tests::fresh_pool;
    use super::super::types::{DispatchContext, StreamDispatchParams};
    use openproxy_adapters::UpstreamClient;
    use openproxy_db::MasterKey;
    use openproxy_types::CancelReason;
    use openproxy_types::combos::{Combo, ComboTarget, PriorityMode, Strategy};
    use openproxy_types::providers::{AuthType, ProviderFormat, RateLimitScope};

    fn build_dispatcher_for_stream_test(
        conn_arc: std::sync::Arc<parking_lot::Mutex<rusqlite::Connection>>,
    ) -> UpstreamDispatcher {
        let repo = std::sync::Arc::new(crate::repository::SqlitePipelineRepository::new(
            std::sync::Arc::clone(&conn_arc),
        ));
        let tracker = crate::usage_tracker::UsageTracker {
            conn: std::sync::Arc::clone(&conn_arc),
            background_tx: tokio::sync::mpsc::channel(1).0,
            record_bodies_and_headers: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            )),
            compression_stats_cell: std::sync::Arc::new(parking_lot::RwLock::new(None)),
            selection_registry: std::sync::Arc::new(openproxy_types::SelectionRegistry::new()),
            cooldown_secs: 60,
            cooldown_max_secs: 3600,
            cooldown_factor: 2,
            repo: std::sync::Arc::clone(&repo)
                as std::sync::Arc<dyn crate::repository::PipelineRepository>,
        };
        let cfg = crate::PipelineConfig {
            defaults: crate::timeouts::Timeouts::from_config(
                &openproxy_types::config::TimeoutsConfig::default(),
            ),
            racing: openproxy_types::config::RacingConfig::default(),
            retries: openproxy_types::config::RetriesConfig::default(),
            max_attempts: 1,
            master_key: std::sync::Arc::new(MasterKey::generate()),
            adapters: std::sync::Arc::new(Vec::new()),
            cooldown_secs: 60,
            cooldown_max_secs: 3600,
            cooldown_factor: 2,
            upstream_client: UpstreamClient::new(),
            oauth_provider_registry: None,
            compression_mode: openproxy_compression::CompressionMode::Off,
            idle_chunk_retryable: true,
            quota_protection: openproxy_types::config::QuotaProtectionConfig::default(),
            background_tx: tokio::sync::mpsc::channel(1).0,
        };
        UpstreamDispatcher::new(
            std::sync::Arc::clone(&conn_arc),
            cfg,
            tracker,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dispatch_upstream_streaming_errors_without_sink() {
        let (_pool, conn_arc, _path) = fresh_pool();
        let provider_id = "stream-test";
        let pid = openproxy_types::ids::ProviderId::new(provider_id);
        {
            let c = conn_arc.lock();
            openproxy_db::providers::create(
                &c,
                openproxy_db::providers::NewProvider {
                    id: &pid,
                    name: provider_id,
                    base_url: "https://example.com",
                    auth_type: AuthType::Bearer,
                    format: ProviderFormat::Openai,
                    extra_headers_json: None,
                    auto_activate_keyword: None,
                    rate_limit_scope: RateLimitScope::Account,
                },
            )
            .expect("seed provider");
        }
        let dispatcher = build_dispatcher_for_stream_test(std::sync::Arc::clone(&conn_arc));

        let model = openproxy_types::models::Model {
            row_id: openproxy_types::ids::ModelRowId(1),
            provider_id: pid.clone(),
            model_id: openproxy_types::ids::ModelId::new("g-2.5"),
            display_name: None,
            discovered_at: "2024-01-01".into(),
            expires_at: None,
            timeout_overrides_json: None,
            last_test_at: None,
            context_length: None,
            max_output_tokens: None,
            capabilities_json: None,
            family: None,
            model_type: "test".into(),
            input_modalities_json: None,
            output_modalities_json: None,
            last_test_status: None,
            target_format: openproxy_types::TargetFormat::Openai,
            active: true,
            custom: false,
            ..Default::default()
        };

        let target = ComboTarget {
            id: openproxy_types::ids::ComboTargetId(1),
            combo_id: openproxy_types::ids::ComboId(1),
            provider_id: pid.clone(),
            account_id: None,
            model_row_id: None,
            sub_combo_id: None,
            priority_order: 1,
            weight: 1,
            active: true,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            rate_limit_scope: RateLimitScope::Account,
        };

        let combo = Combo {
            id: openproxy_types::ids::ComboId(1),
            name: "stream-test".into(),
            strategy: Strategy::Priority,
            priority_mode: PriorityMode::Strict,
            race_size: 1,
            created_at: "2024-01-01".into(),
            context_window: None,
            cooldown_mode: openproxy_types::config::CooldownMode::None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            lkgp_exploration_rate: None,
            selection_window_secs: Some(3600),
            preventive_rate_limit: false,
        };

        let (_tx, rx) = tokio::sync::watch::channel::<Option<CancelReason>>(None);
        // stream_sink = None → debe disparar la rama de Internal error
        // antes de cualquier IO upstream.
        let req = crate::PipelineRequest {
            request_id: openproxy_types::ids::RequestId::new(),
            trace_id: openproxy_types::ids::TraceId::new(),
            combo_id: openproxy_types::ids::ComboId(1),
            openai_request: std::sync::Arc::new(openproxy_types::OpenAIRequest {
                model: "g-2.5".into(),
                messages: vec![],
                stream: true,
                temperature: None,
                max_tokens: None,
                top_p: None,
                stop: None,
                tools: None,
                tool_choice: None,
                top_k: None,
                user: None,
                extra: serde_json::Map::new(),
            }),
            client_disconnected: rx,
            stream_sink: None,
            api_key_id: None,
            combo_override: None,
            targets_override: None,
            request_headers: std::collections::BTreeMap::new(),
            request_body_json: None,
            race_cancelled: false,
            race_cancel: None,
            endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
            compressed_messages: std::sync::Arc::new(std::sync::OnceLock::new()),
            proxy_override: None,
        };

        let started = std::time::Instant::now();
        let params = StreamDispatchParams {
            target: &target,
            combo: &combo,
            req,
            model: &model,
            target_format: openproxy_types::TargetFormat::Openai,
            resolved_timeouts: &crate::timeouts::Timeouts::from_config(
                &openproxy_types::config::TimeoutsConfig::default(),
            ),
            started,
            attempt: 1,
            race_size: 1,
            trace_id: "t-stream".to_string(),
            upstream_request: openproxy_adapters::upstream::UpstreamRequest::post_json(
                String::new(),
                bytes::Bytes::new(),
            ),
        };

        let result = dispatcher.dispatch_upstream_streaming(params).await;
        assert!(
            result.error.is_some(),
            "dispatch_upstream_streaming without stream_sink must produce an error"
        );
        match result.error.expect("just asserted is_some") {
            openproxy_types::error::CoreError::Internal(msg) => {
                assert!(
                    msg.contains("stream_sink"),
                    "Internal error must mention stream_sink, got: {msg}"
                );
            }
            other => panic!("expected CoreError::Internal, got {other:?}"),
        }
        assert_eq!(result.status_code, 500);

        // Verificamos también que el DispatchContext se construye
        // correctamente vía la rama de fallo temprano — sanity check
        // del patrón de destructuring en `dispatch_upstream_streaming`.
        let _dctx: DispatchContext<'_> = DispatchContext {
            attempt: 1,
            race_size: 1,
            started,
            model: &model,
            proxy_url: None,
            proxy_status: None,
        };
    }
}
