use crate::adapters::ProviderAdapter;
use crate::adapters::AdapterFormat;
use crate::compression::stats::CompressionStats;
use crate::compression::CompressionMode;
use crate::timeouts::ModelTimeoutOverrides;
use crate::error::CoreError;
use crate::pipeline::context::PipelineContext;
use crate::pipeline::stage::{PipelineNext, PipelineStage};
use crate::pipeline::{FailureContext, PipelineResult};
use crate::retry::RetryPolicy;
use crate::timeouts;
use async_trait::async_trait;
use std::sync::Arc;

#[derive(Clone, Copy)]
pub struct OAuthRefreshStage;


impl PipelineStage for OAuthRefreshStage {
    async fn execute(
        &self,
        ctx: &mut PipelineContext,
        next: PipelineNext<'_>,
    ) -> Result<PipelineResult, CoreError> {
        let current = ctx.current_target.as_mut().expect("current_target must be set");
        let target = &current.target;
        
        if let Some(account_id) = target.account_id {
            if let Some(custom_meta) = &mut current.custom_meta {
                if let Some(refresh_token) = &custom_meta.maybe_refresh {
                    if let Some(registry) = ctx.pipeline.config.oauth_provider_registry.as_ref() {
                        if let Some(provider) = registry.get(target.provider_id.as_str()) {
                            let provider_id_str = target.provider_id.as_str();
                            tracing::info!(
                                account = account_id.0,
                                provider = provider_id_str,
                                "pipeline: proactive OAuth token refresh"
                            );
                            match crate::oauth::TokenRefreshCoordinator::global()
                                .refresh_and_store(
                                    provider_id_str,
                                    provider,
                                    refresh_token,
                                    &ctx.pipeline.config.upstream_client,
                                    account_id,
                                    crate::oauth::DbRef::Connection(&ctx.pipeline.conn),
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
                    }
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
                        Arc::clone(&ctx.req),
                        ctx.combo.as_ref().unwrap(),
                        &current.target,
                        FailureContext {
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

        let resolved_timeouts = timeouts::resolve(&ctx.pipeline.config.defaults, Some(&model_overrides));
        
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
        let adapter = match ctx.pipeline.config.adapters.iter().find(|a| a.id() == &current.target.provider_id) {
            Some(a) => a.clone(),
            None => {
                let err = CoreError::ProviderNotFound(current.target.provider_id.to_string());
                return Ok(ctx.pipeline.record_and_fail(
                    Arc::clone(&ctx.req),
                    ctx.combo.as_ref().unwrap(),
                    &current.target,
                    FailureContext {
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
            AdapterFormat::Openai => crate::models::TargetFormat::Openai,
            AdapterFormat::Anthropic => crate::models::TargetFormat::Anthropic,
            AdapterFormat::Mixed => current.model.target_format,
            AdapterFormat::Gemini => crate::models::TargetFormat::Gemini,
            AdapterFormat::Responses => crate::models::TargetFormat::Responses,
        };

        let stream = if !ctx.req.openai_request.stream && ctx.req.stream_sink.is_some() {
            true
        } else {
            ctx.req.openai_request.stream
        };

        let cloned_messages_ref = ctx.req.compressed_messages.get_or_init(|| {
            if crate::compression::would_compress(&ctx.req.openai_request.messages, ctx.pipeline.config.compression_mode) {
                let mut msgs = ctx.req.openai_request.messages.clone();
                let stats = crate::compression::apply_compression(&mut msgs, ctx.pipeline.config.compression_mode);
                *ctx.pipeline.compression_stats_cell.write() = Some(stats);
                Some(msgs)
            } else {
                *ctx.pipeline.compression_stats_cell.write() = Some(crate::compression::stats::CompressionStats::empty());
                None
            }
        });

        let messages_ref = cloned_messages_ref
            .as_deref()
            .unwrap_or(&ctx.req.openai_request.messages);

        let formatter = crate::pipeline::formatting::get_formatter(target_format);
        let body_bytes = match formatter.format_request(&ctx.req, &current.model, messages_ref, stream, &adapter) {
            Ok(b) => b,
            Err(e) => {
                return Ok(ctx.pipeline.record_and_fail(
                    Arc::clone(&ctx.req),
                    ctx.combo.as_ref().unwrap(),
                    &current.target,
                    FailureContext {
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
        next: PipelineNext<'_>,
    ) -> Result<PipelineResult, CoreError> {
        let current = ctx.current_target.as_ref().unwrap();
        let target = &current.target;
        let model = &current.model;
        let attempt = ctx.current_target_attempt;
        let race_size = ctx.race_size;
        let started = ctx.started.unwrap();
        let trace_id = ctx.trace_id.clone();
        
        let adapter = match ctx.pipeline.config.adapters.iter().find(|a| a.id() == &target.provider_id) {
            Some(a) => a.clone(),
            None => {
                let err = CoreError::ProviderNotFound(target.provider_id.to_string());
                return Ok(ctx.pipeline.record_and_fail(
                    Arc::clone(&ctx.req),
                    ctx.combo.as_ref().unwrap(),
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
        
        if let Some(cancel) = &ctx.race_cancel {
            if cancel.is_cancelled() {
                return Ok(ctx.pipeline.record_and_fail_with_trace_id(
                    Arc::clone(&ctx.req),
                    ctx.combo.as_ref().unwrap(),
                    target,
                    FailureContext {
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
        }

        let (api_key, account_label) = match ctx.pipeline.resolve_target_api_key_and_label(target) {
            Ok(v) => v,
            Err(e) => {
                return Ok(ctx.pipeline.record_and_fail(
                    Arc::clone(&ctx.req),
                    ctx.combo.as_ref().unwrap(),
                    target,
                    FailureContext {
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

        let target_format = ctx.target_format.unwrap();
        let account_label_str = account_label.as_deref().unwrap_or("");
        let url = adapter.build_chat_url_for_account(target_format, &model.model_id, account_label_str);
        let headers = adapter.build_headers(&api_key, target_format, &model.model_id);
        
        let compression_stats_at_connecting = ctx.pipeline.compression_stats_cell.read().clone();
        crate::usage::publish_stage_event(crate::usage::StageEvent {
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
            endpoint_kind: crate::endpoint::EndpointKind::Chat,
        });

        let body_bytes = ctx.body_bytes.clone().unwrap();
        let resolved_timeouts = ctx.resolved_timeouts.clone().unwrap();

        let result = ctx.pipeline.dispatcher
            .dispatch_upstream(
                target,
                ctx.combo.as_ref().unwrap(),
                Arc::clone(&ctx.req),
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
                Some(e) if RetryPolicy::is_retryable(e, ctx.pipeline.config.idle_chunk_retryable) => {
                    let outcome = ctx.pipeline.circuit_breaker.record_failure_outcome(aid);
                    if outcome.just_opened {
                        let provider_id_str = target.provider_id.to_string();
                        let model_id_str = model.model_id.as_str().to_string();
                        let combo_target_id = target.id.0;
                        let dedup_key = format!("{}:{}", crate::notifications::CODE_CIRCUIT_OPEN, aid.0);
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
                        let conn = Arc::clone(&ctx.pipeline.conn);
                        tokio::task::spawn_blocking(move || {
                            let conn = conn.lock();
                            let _ = crate::notifications::insert_and_broadcast(
                                &conn,
                                crate::notifications::KIND_SYSTEM,
                                &payload,
                                Some(&dedup_key),
                                Some(&provider_id_str),
                            );
                        });
                    }
                }
                _ => {
                    ctx.pipeline.circuit_breaker.record_success(aid);
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
        let current = ctx.current_target.as_mut().expect("current_target must be set");
        let target = &current.target;
        let adapter = match ctx.pipeline.config.adapters.iter().find(|a| a.id() == &target.provider_id) {
            Some(a) => a.clone(),
            None => {
                let err = CoreError::ProviderNotFound(target.provider_id.to_string());
                return Ok(ctx.pipeline.record_and_fail_with_trace_id(
                    Arc::clone(&ctx.req),
                    ctx.combo.as_ref().unwrap(),
                    target,
                    FailureContext {
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

        if let Some(result) = adapter
            .execute_custom(
                &ctx.pipeline.config.upstream_client,
                Arc::clone(&ctx.req),
                current,
                Some(crate::adapters::CustomExecutionContext {
                    conn: Arc::clone(&ctx.pipeline.conn),
                    cooldown_mode: ctx.combo.as_ref().unwrap().cooldown_mode,
                    cooldown_base_secs: ctx.combo.as_ref().unwrap()
                        .cooldown_base_secs
                        .unwrap_or(ctx.pipeline.config.cooldown_secs),
                    cooldown_max_secs: ctx.combo.as_ref().unwrap()
                        .cooldown_max_secs
                        .unwrap_or(ctx.pipeline.config.cooldown_max_secs),
                    cooldown_factor: ctx.combo.as_ref().unwrap().cooldown_factor.unwrap_or(ctx.pipeline.config.cooldown_factor),
                }),
            )
            .await
        {
            return match result {
                Ok(response) => {
                    let total_ms = ctx.started.unwrap().elapsed().as_millis() as u64;
                    let usage_tuple = match crate::pipeline::usage_tracker::UsageRecordBuilder::new(
                        &ctx.pipeline.tracker,
                        Arc::clone(&ctx.req),
                        ctx.combo.as_ref().unwrap(),
                        target,
                    )
                    .model_opt(Some(&current.model))
                    .err_opt(None)
                    .connect_ms_opt(None)
                    .ttft_ms_opt(None)
                    .total_ms(total_ms)
                    .status_code(200)
                    .attempt(ctx.current_target_attempt)
                    .race_size(ctx.race_size)
                    .trace_id(ctx.trace_id.clone())
                    .prompt_tokens_opt(response.usage.as_ref().map(|u| u.prompt_tokens))
                    .completion_tokens_opt(response.usage.as_ref().map(|u| u.completion_tokens))
                    .request_body_json(None)
                    .response_body_json(None)
                    .request_headers(None)
                    .response_headers(None)
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
                    Ok(PipelineResult {
                        status_code: 200,
                        error: None,
                        final_response: Some(response),
                        attempts: ctx.current_target_attempt,
                        usage_tuple,
                    })
                }
                Err(e) => {
                    if let CoreError::UpstreamError { status: 401, .. } = &e
                        && let Some(account_id) = target.account_id
                    {
                        let provider_id_str = target.provider_id.to_string();
                        let dedup_key = format!(
                            "{}:{}",
                            crate::notifications::CODE_OAUTH_EXPIRED,
                            account_id.0
                        );
                        let payload = serde_json::json!({
                            "code": crate::notifications::CODE_OAUTH_EXPIRED,
                            "message": format!("OAuth token for account {} on {} rejected by upstream (HTTP 401)", account_id.0, provider_id_str),
                            "provider_id": &provider_id_str,
                            "details": {
                                "account_id": account_id.0,
                                "provider_id": &provider_id_str,
                                "reason": "upstream_401",
                            },
                        });
                        let conn = Arc::clone(&ctx.pipeline.conn);
                        tokio::task::spawn_blocking(move || {
                            let conn = conn.lock();
                            let _ = crate::notifications::insert_and_broadcast(
                                &conn,
                                crate::notifications::KIND_SYSTEM,
                                &payload,
                                Some(&dedup_key),
                                Some(&provider_id_str),
                            );
                        });
                    }
                    Ok(ctx.pipeline.record_and_fail_with_trace_id(
                        Arc::clone(&ctx.req),
                        ctx.combo.as_ref().unwrap(),
                        target,
                        FailureContext {
                            attempt: ctx.current_target_attempt,
                            race_size: ctx.race_size,
                            err: &e,
                            started: ctx.started.unwrap(),
                            model: Some(&current.model),
                            connect_ms: None,
                            ttft_ms: None,
                            status_code: e.http_status(),
                        },
                        ctx.trace_id.clone(),
                    ))
                }
            };
        }
        next.execute(ctx).await
    }
}
