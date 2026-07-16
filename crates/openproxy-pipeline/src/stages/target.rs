use openproxy_adapters::adapters::AdapterFormat;
use openproxy_types::error::CoreError;
use crate::context::PipelineContext;
use crate::stage::{PipelineNext, PipelineStage};
use crate::{FailureContext, PipelineResult};
use crate::retry::RetryPolicy;
use crate::timeouts;
use crate::timeouts::ModelTimeoutOverrides;

#[derive(Clone, Copy)]
pub struct OAuthRefreshStage;

impl PipelineStage for OAuthRefreshStage {
    async fn execute(
        &self,
        ctx: &mut PipelineContext,
        next: PipelineNext<'_>,
    ) -> Result<PipelineResult, CoreError> {
        let current = ctx
            .current_target
            .as_mut()
            .expect("current_target must be set");
        let target = &current.target;

        if let Some(account_id) = target.account_id
            && let Some(custom_meta) = &mut current.custom_meta
            && let Some(refresh_token) = &custom_meta.maybe_refresh
            && let Some(registry) = ctx.pipeline.config.oauth_provider_registry.as_ref()
        {
            let provider_id_str = target.provider_id.as_str();
            tracing::info!(
                account = account_id.0,
                provider = provider_id_str,
                "pipeline: proactive OAuth token refresh"
            );
            match registry
                .refresh_and_store(
                    provider_id_str,
                    refresh_token,
                    &ctx.pipeline.config.upstream_client,
                    account_id,
                    &ctx.pipeline.conn,
                    &ctx.pipeline.config.master_key,
                )
                .await
            {
                Ok(token) => {
                    custom_meta.access_token = token.access_token;
                }
                Err(e) => {
                    tracing::warn!(account = account_id.0, provider = provider_id_str, error = %e, "pipeline: proactive OAuth refresh failed, continuing with existing token");
                }
            }
        }
        next.execute(ctx).await
    }
}

#[derive(Clone, Copy)]
pub struct TimeoutResolutionStage;

impl PipelineStage for TimeoutResolutionStage {
    async fn execute(
        &self,
        ctx: &mut PipelineContext,
        next: PipelineNext<'_>,
    ) -> Result<PipelineResult, CoreError> {
        let current = ctx.current_target.as_ref().unwrap();
        let model = &current.model;
        let attempt = ctx.current_target_attempt;
        let race_size = ctx.race_size;
        let started = ctx.started.unwrap();

        let model_overrides =
            match ModelTimeoutOverrides::from_json(model.timeout_overrides_json.as_deref()) {
                Ok(o) => o,
                Err(e) => {
                    return Ok(ctx.pipeline.record_and_fail(
                        ctx.req.clone(),
                        ctx.combo.as_ref().unwrap(),
                        &current.target,
                        FailureContext {
                            proxy_url: None,
                            proxy_status: None,
                            attempt,
                            race_size,
                            err: &e,
                            started,
                            model: Some(model),
                            connect_ms: None,
                            ttft_ms: None,
                            status_code: 0,
                        },
                    ));
                }
            };

        let resolved_timeouts =
            timeouts::resolve(&ctx.pipeline.config.defaults, Some(&model_overrides));

        tracing::debug!(
            target_id = current.target.id.0,
            provider = %current.target.provider_id,
            model = %model.model_id.as_str(),
            total_ms = resolved_timeouts.total.as_millis() as u64,
            "resolved timeouts for target"
        );

        ctx.resolved_timeouts = Some(resolved_timeouts);
        next.execute(ctx).await
    }
}

#[derive(Clone, Copy)]
pub struct FormattingStage;

impl PipelineStage for FormattingStage {
    async fn execute(
        &self,
        ctx: &mut PipelineContext,
        next: PipelineNext<'_>,
    ) -> Result<PipelineResult, CoreError> {
        let current = ctx.current_target.as_ref().unwrap();
        let adapter = match ctx
            .pipeline
            .config
            .adapters
            .iter()
            .find(|a| a.id() == &current.target.provider_id)
        {
            Some(a) => a.clone(),
            None => {
                let err = CoreError::ProviderNotFound(current.target.provider_id.to_string());
                return Ok(ctx.pipeline.record_and_fail(
                    ctx.req.clone(),
                    ctx.combo.as_ref().unwrap(),
                    &current.target,
                    FailureContext {
                        proxy_url: None,
                        proxy_status: None,
                        attempt: ctx.current_target_attempt,
                        race_size: ctx.race_size,
                        err: &err,
                        started: ctx.started.unwrap(),
                        model: None,
                        connect_ms: None,
                        ttft_ms: None,
                        status_code: 0,
                    },
                ));
            }
        };

        let target_format = match adapter.format() {
            AdapterFormat::Openai => openproxy_types::TargetFormat::Openai,
            AdapterFormat::Anthropic => openproxy_types::TargetFormat::Anthropic,
            AdapterFormat::Mixed => current.model.target_format,
            AdapterFormat::Gemini => openproxy_types::TargetFormat::Gemini,
            AdapterFormat::Responses => openproxy_types::TargetFormat::Responses,
        };

        let stream = if !ctx.req.openai_request.stream && ctx.req.stream_sink.is_some() {
            true
        } else {
            ctx.req.openai_request.stream
        };

        let cloned_messages_ref = ctx.req.compressed_messages.get_or_init(|| {
            if openproxy_compression::would_compress(
                &ctx.req.openai_request.messages,
                ctx.pipeline.config.compression_mode,
            ) {
                let mut msgs = ctx.req.openai_request.messages.clone();
                let stats = openproxy_compression::apply_compression(
                    &mut msgs,
                    ctx.pipeline.config.compression_mode,
                );
                *ctx.pipeline.compression_stats_cell.write() = Some(stats);
                Some(msgs)
            } else {
                *ctx.pipeline.compression_stats_cell.write() =
                    Some(openproxy_compression::stats::CompressionStats::empty());
                None
            }
        });

        let messages_ref = cloned_messages_ref
            .as_deref()
            .unwrap_or(&ctx.req.openai_request.messages);

        let formatter = crate::formatting::get_formatter(target_format);
        let body_bytes = match formatter.format_request(
            &ctx.req,
            &current.model,
            messages_ref,
            stream,
            &adapter,
        ).and_then(|body| adapter.wrap_request_body(body, target_format, &current.model.model_id, current)) {
            Ok(b) => b,
            Err(e) => {
                return Ok(ctx.pipeline.record_and_fail(
                    ctx.req.clone(),
                    ctx.combo.as_ref().unwrap(),
                    &current.target,
                    FailureContext {
                        proxy_url: None,
                        proxy_status: None,
                        attempt: ctx.current_target_attempt,
                        race_size: ctx.race_size,
                        err: &e,
                        started: ctx.started.unwrap(),
                        model: Some(&current.model),
                        connect_ms: None,
                        ttft_ms: None,
                        status_code: 0,
                    },
                ));
            }
        };

        ctx.target_format = Some(target_format);
        ctx.body_bytes = Some(body_bytes);
        next.execute(ctx).await
    }
}

#[derive(Clone, Copy)]
pub struct DispatchStage;

impl PipelineStage for DispatchStage {
    async fn execute(
        &self,
        ctx: &mut PipelineContext,
        _next: PipelineNext<'_>,
    ) -> Result<PipelineResult, CoreError> {
        let current = ctx.current_target.as_ref().unwrap();
        let target = &current.target;
        let model = &current.model;
        let attempt = ctx.current_target_attempt;
        let race_size = ctx.race_size;
        let started = ctx.started.unwrap();
        let trace_id = ctx.trace_id.clone();

        let adapter = match ctx
            .pipeline
            .config
            .adapters
            .iter()
            .find(|a| a.id() == &target.provider_id)
        {
            Some(a) => a.clone(),
            None => {
                let err = CoreError::ProviderNotFound(target.provider_id.to_string());
                return Ok(ctx.pipeline.record_and_fail(
                    ctx.req.clone(),
                    ctx.combo.as_ref().unwrap(),
                    target,
                    FailureContext {
                        proxy_url: None,
                        proxy_status: None,
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

        if let Some(cancel) = &ctx.race_cancel
            && cancel.is_cancelled()
        {
            return Ok(ctx.pipeline.record_and_fail_with_trace_id(
                ctx.req.clone(),
                ctx.combo.as_ref().unwrap(),
                target,
                FailureContext {
                    proxy_url: None,
                    proxy_status: None,
                    attempt,
                    race_size,
                    err: &CoreError::RaceLost,
                    started,
                    model: Some(model),
                    connect_ms: None,
                    ttft_ms: None,
                    status_code: CoreError::RaceLost.http_status(),
                },
                trace_id,
            ));
        }

        let api_key = current
            .custom_meta
            .as_ref()
            .map(|m| m.access_token.clone())
            .unwrap_or_else(|| current.api_key.clone());
        let account_label = current.api_key_label.clone();

        let target_format = ctx.target_format.unwrap();
        let account_label_str = account_label.as_deref().unwrap_or("");
        let url =
            adapter.build_chat_url_for_account(target_format, &model.model_id, account_label_str);
        let headers = adapter.build_headers(&api_key, target_format, &model.model_id);

        let compression_stats_at_connecting = ctx.pipeline.compression_stats_cell.read().clone();
        openproxy_types::usage::publish_stage_event(openproxy_types::usage::StageEvent {
            request_id: ctx.req.request_id.to_string(),
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
            endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
        });

        let body_bytes = ctx.body_bytes.clone().unwrap();
        let resolved_timeouts = ctx.resolved_timeouts.unwrap();

        let result = ctx
            .pipeline
            .dispatcher
            .dispatch_upstream(
                target,
                ctx.combo.as_ref().unwrap(),
                ctx.req.clone(),
                model,
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
                Some(e)
                    if RetryPolicy::is_retryable(e, ctx.pipeline.config.idle_chunk_retryable) =>
                {
                    let key = if target.rate_limit_scope == openproxy_types::providers::RateLimitScope::Model
                    {
                        crate::circuit_breaker::CircuitBreakerKey::Model(
                            aid,
                            target.model_row_id.expect("flattened"),
                        )
                    } else {
                        crate::circuit_breaker::CircuitBreakerKey::Account(aid)
                    };
                    let outcome = ctx.pipeline.circuit_breaker.record_failure_outcome(key);
                    if outcome.just_opened {
                        let provider_id_str = target.provider_id.to_string();
                        let model_id_str = model.model_id.as_str().to_string();
                        let combo_target_id = target.id.0;
                        let dedup_key =
                            format!("circuit_open:{}", aid.0);
                        let payload = serde_json::json!({
                            "code": "circuit_open",
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
                        let repo = ctx.pipeline.repo();
                        let provider_id_str_clone = provider_id_str.clone();
                        tokio::task::spawn_blocking(move || {
                            let _ = repo.insert_and_broadcast_notification(
                                "system",
                                &payload,
                                Some(&dedup_key),
                                Some(&provider_id_str_clone),
                            );
                        });
                    }
                }
                _ => {
                    let key = if target.rate_limit_scope == openproxy_types::providers::RateLimitScope::Model
                    {
                        crate::circuit_breaker::CircuitBreakerKey::Model(
                            aid,
                            target.model_row_id.expect("flattened"),
                        )
                    } else {
                        crate::circuit_breaker::CircuitBreakerKey::Account(aid)
                    };
                    ctx.pipeline.circuit_breaker.record_success(key);
                }
            }
        }

        // Do not call next.execute here. We are the final stage of target execution.
        Ok(result)
    }
}

#[derive(Clone, Copy)]
pub struct CustomAdapterStage;

impl PipelineStage for CustomAdapterStage {
    async fn execute(
        &self,
        ctx: &mut PipelineContext,
        next: PipelineNext<'_>,
    ) -> Result<PipelineResult, CoreError> {
        let current = ctx
            .current_target
            .as_mut()
            .expect("current_target must be set");
        let target = &current.target;
        let _adapter = match ctx
            .pipeline
            .config
            .adapters
            .iter()
            .find(|a| a.id() == &target.provider_id)
        {
            Some(a) => a.clone(),
            None => {
                let err = CoreError::ProviderNotFound(target.provider_id.to_string());
                return Ok(ctx.pipeline.record_and_fail_with_trace_id(
                    ctx.req.clone(),
                    ctx.combo.as_ref().unwrap(),
                    target,
                    FailureContext {
                        proxy_url: None,
                        proxy_status: None,
                        attempt: ctx.current_target_attempt,
                        race_size: ctx.race_size,
                        err: &err,
                        started: ctx.started.unwrap(),
                        model: Some(&current.model),
                        connect_ms: None,
                        ttft_ms: None,
                        status_code: 0,
                    },
                    ctx.trace_id.clone(),
                ));
            }
        };

        next.execute(ctx).await
    }
}
