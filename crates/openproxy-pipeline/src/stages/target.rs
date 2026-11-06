use crate::context::PipelineContext;
use crate::stage::{PipelineNext, PipelineStage};
use crate::timeouts;
use crate::timeouts::ModelTimeoutOverrides;
use crate::{FailureContext, PipelineResult};
use openproxy_types::error::CoreError;

macro_rules! fail_stage {
    ($ctx:expr, $target:expr, $err:expr, $model:expr) => {
        fail_stage!($ctx, $target, $err, $model, 0)
    };
    ($ctx:expr, $target:expr, $err:expr, $model:expr, $status:expr) => {{
        let combo = $ctx
            .combo
            .as_ref()
            .ok_or_else(|| CoreError::Internal("missing combo in pipeline context".into()))?;
        let fail_ctx = FailureContext {
            proxy_url: None,
            proxy_status: None,
            attempt: $ctx.current_target_attempt,
            race_size: $ctx.race_size,
            err: $err,
            started: $ctx.started.unwrap_or_else(std::time::Instant::now),
            model: $model,
            connect_ms: None,
            ttft_ms: None,
            status_code: $status,
        };
        Ok($ctx
            .pipeline
            .record_and_fail($ctx.req.clone(), combo, $target, fail_ctx)
            .await)
    }};
    (with_trace; $ctx:expr, $target:expr, $err:expr, $model:expr, $status:expr) => {{
        let combo = $ctx
            .combo
            .as_ref()
            .ok_or_else(|| CoreError::Internal("missing combo in pipeline context".into()))?;
        let fail_ctx = FailureContext {
            proxy_url: None,
            proxy_status: None,
            attempt: $ctx.current_target_attempt,
            race_size: $ctx.race_size,
            err: $err,
            started: $ctx.started.unwrap_or_else(std::time::Instant::now),
            model: $model,
            connect_ms: None,
            ttft_ms: None,
            status_code: $status,
        };
        Ok($ctx
            .pipeline
            .record_and_fail_with_trace_id(
                $ctx.req.clone(),
                combo,
                $target,
                fail_ctx,
                $ctx.trace_id.clone(),
            )
            .await)
    }};
}

#[derive(Clone, Copy)]
pub struct OAuthRefreshStage;

impl PipelineStage for OAuthRefreshStage {
    async fn execute(
        &self,
        ctx: &mut PipelineContext,
        next: PipelineNext<'_>,
    ) -> Result<PipelineResult, CoreError> {
        let Some(current) = ctx.current_target.as_mut() else {
            return Err(CoreError::Internal(
                "missing current_target in pipeline context".into(),
            ));
        };
        try_proactive_oauth_refresh(&ctx.pipeline, current).await;
        next.execute(ctx).await
    }
}

async fn try_proactive_oauth_refresh(
    pipeline: &crate::Pipeline,
    current: &mut crate::context::ResolvedTarget,
) {
    let Some(account_id) = current.target.account_id else {
        return;
    };
    let Some(custom_meta) = current.custom_meta.as_mut() else {
        return;
    };
    let Some(refresh_token) = custom_meta.maybe_refresh.as_ref() else {
        return;
    };
    let Some(registry) = pipeline.config.oauth_provider_registry.as_ref() else {
        return;
    };

    let provider_id_str = current.target.provider_id.as_str();
    tracing::info!(
        account = account_id.0,
        provider = provider_id_str,
        "pipeline: proactive OAuth token refresh"
    );
    let refresh_res = match pipeline.db_pool.as_ref() {
        Some(pool) => {
            registry
                .refresh_and_store(
                    provider_id_str,
                    refresh_token,
                    &pipeline.config.upstream_client,
                    account_id,
                    Some(pool),
                    &pipeline.config.master_key,
                )
                .await
        }
        None => {
            registry
                .refresh_and_store_shared(
                    provider_id_str,
                    refresh_token,
                    &pipeline.config.upstream_client,
                    account_id,
                    &pipeline.conn,
                    &pipeline.config.master_key,
                )
                .await
        }
    };
    match refresh_res {
        Ok(token) => {
            custom_meta.access_token = token.access_token;
            custom_meta.maybe_refresh = None;
        }
        Err(e) => {
            tracing::warn!(
                account = account_id.0,
                provider = provider_id_str,
                error = %e,
                "pipeline: proactive OAuth refresh failed, continuing with existing token"
            );
            custom_meta.maybe_refresh = None;
        }
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
        let current = ctx.current_target.as_ref().ok_or_else(|| {
            CoreError::Internal("missing current_target in pipeline context".into())
        })?;
        let cloned_model = current.model.clone();
        let model = &cloned_model;

        let model_overrides =
            match ModelTimeoutOverrides::from_json(model.timeout_overrides_json.as_deref()) {
                Ok(o) => o,
                Err(e) => return fail_stage!(ctx, &current.target, &e, Some(model)),
            };

        let mut resolved_timeouts =
            timeouts::resolve(&ctx.pipeline.config.defaults, Some(&model_overrides));

        let provider_str = current.target.provider_id.as_str();
        if provider_str == "antigravity" || provider_str == "agy" {
            resolved_timeouts = timeouts::apply_antigravity_floor(resolved_timeouts);
        }

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
        let current = ctx.current_target.as_ref().ok_or_else(|| {
            CoreError::Internal("missing current_target in pipeline context".into())
        })?;
        let Some(adapter) = crate::context::pinned_adapter(
            &ctx.req,
            &ctx.pipeline.config.adapters,
            &current.target.provider_id,
        ) else {
            let err = CoreError::ProviderNotFound(current.target.provider_id.to_string());
            return fail_stage!(ctx, &current.target, &err, None);
        };

        let mut target_format = resolve_target_format(&adapter, current.model.target_format);
        #[allow(clippy::collapsible_if)]
        if current.target.provider_id.as_str() == "zai" {
            if let Some(acc_id) = current.target.account_id {
                let key = std::sync::Arc::clone(&ctx.pipeline.config.master_key);
                if let Ok(Some(acc)) = ctx
                    .pipeline
                    .async_repo()
                    .run(move |repo| repo.get_account(acc_id, &key))
                    .await
                {
                    let now = openproxy_types::now_unix_secs_str()
                        .parse::<u64>()
                        .unwrap_or(0);
                    let fallback = current
                        .custom_meta
                        .as_ref()
                        .map_or(current.api_key.as_str(), |m| m.access_token.as_str());
                    #[allow(clippy::collapsible_if)]
                    if let Ok(route) =
                        openproxy_adapters::adapters::zai::resolve_zai_inference_route(
                            &acc,
                            fallback,
                            current.model.model_id.as_str(),
                            now,
                            &adapter.config().base_url,
                        )
                    {
                        if route.source == openproxy_types::quota::QuotaSource::AutoclawStarter {
                            target_format = openproxy_types::TargetFormat::Openai;
                        }
                    }
                }
            }
        }
        let model_streaming = current.model.capabilities().and_then(|c| c.streaming);
        let adapter_streaming = adapter.forced_streaming();
        let stream = resolve_upstream_streaming(model_streaming, adapter_streaming);

        // Resolve and inject system prefix for this target if configured
        // (Supports inheritance: if this target was flattened from a parent sub-combo target,
        // it checks its own prefixes first; if empty, inherits the parent target's prefixes).
        let prefix_text = resolve_system_prefix_for_target(
            ctx,
            current.target.id,
            current.target.inherited_target_id,
        )
        .await;
        if let Some(ref p) = prefix_text {
            *ctx.req.active_system_prefix.lock() = Some(p.clone());
        }

        let base_messages_ref = prepare_messages_for_formatting(ctx);
        let messages_storage: Vec<openproxy_types::OpenAIMessage>;
        let messages_ref: &[openproxy_types::OpenAIMessage] = if let Some(ref prefix) = prefix_text
        {
            let mut msgs = Vec::with_capacity(base_messages_ref.len() + 1);
            msgs.push(openproxy_types::OpenAIMessage {
                role: "system".to_string(),
                content: Some(serde_json::Value::String(prefix.clone())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: Default::default(),
            });
            msgs.extend(base_messages_ref.iter().cloned());
            messages_storage = msgs;
            &messages_storage
        } else {
            base_messages_ref
        };

        let policy = crate::policy::EffectiveTargetPolicy::resolve(
            ctx.combo.as_ref(),
            &current.target,
            &current.model,
        );
        let effective_model = policy.apply_to_model(&current.model);

        let mut req_override;
        let req_ref = if current.target.thinking_effort.is_some()
            || (ctx.req.openai_request.max_tokens.is_none()
                && !ctx
                    .req
                    .openai_request
                    .extra
                    .contains_key("max_completion_tokens")
                && policy.max_output_tokens.is_some())
        {
            req_override = ctx.req.clone();
            req_override.openai_request = policy.normalize_request(&ctx.req.openai_request);
            &req_override
        } else {
            &ctx.req
        };

        let formatter = crate::formatting::get_formatter(target_format);
        let (effective_messages_ref, synthetic_thread_header) =
            crate::message_threads::resolve_synthetic_thread_plan(
                ctx,
                target_format,
                adapter.id().as_str(),
                messages_ref,
            );

        let body_bytes = match formatter
            .format_request(
                req_ref,
                &effective_model,
                effective_messages_ref,
                stream,
                &adapter,
            )
            .and_then(|body| {
                if let Some(thread_obj) = synthetic_thread_header.as_ref() {
                    openproxy_adapters::adapters::traits::patch_json_request_body(body, |obj| {
                        obj.insert("thread".to_string(), thread_obj.clone());
                    })
                } else {
                    Ok(body)
                }
            })
            .and_then(|body| {
                adapter.wrap_request_body(body, target_format, &current.model.model_id, current)
            })
            .and_then(|body| {
                // Devin bodies are protobuf envelopes by this point — JSON
                // param stripping would fail the parse on binary input.
                if target_format == openproxy_types::TargetFormat::Devin {
                    return Ok(body);
                }
                crate::formatting::strip_provider_configured_params_from_body(
                    body,
                    &adapter.config().extra_headers,
                )
            })
            .and_then(|body| enforce_stream_body(body, target_format, stream))
        {
            Ok(b) => b,
            Err(e) => return fail_stage!(ctx, &current.target, &e, Some(&current.model)),
        };

        ctx.target_format = Some(target_format);
        ctx.body_bytes = Some(body_bytes);
        ctx.is_streaming = Some(stream);
        next.execute(ctx).await
    }
}

/// Unary at either scope is an explicit veto. Auto always streams upstream,
/// independently of the downstream response format requested by the client.
fn resolve_upstream_streaming(model: Option<bool>, provider: Option<bool>) -> bool {
    model != Some(false) && provider != Some(false)
}

/// Provider wrappers and parameter stripping cannot silently override transport
/// policy. Apply it before request finalization/signing, in the native dialect.
fn enforce_stream_body(
    body: bytes::Bytes,
    format: openproxy_types::TargetFormat,
    stream: bool,
) -> Result<bytes::Bytes, CoreError> {
    if matches!(
        format,
        openproxy_types::TargetFormat::Gemini | openproxy_types::TargetFormat::Devin
    ) {
        // Gemini selects streaming through the URL, not JSON. Devin's body is
        // a Connect-RPC protobuf envelope: JSON patching would corrupt it and
        // the RPC has no stream flag (the endpoint is always server-streaming).
        return Ok(body);
    }
    openproxy_adapters::adapters::traits::patch_json_request_body(body, |obj| {
        if format == openproxy_types::TargetFormat::CommandCodeGo {
            if let Some(params) = obj
                .get_mut("params")
                .and_then(serde_json::Value::as_object_mut)
            {
                params.insert("stream".into(), serde_json::Value::Bool(stream));
            }
        } else {
            obj.insert("stream".into(), serde_json::Value::Bool(stream));
            if !stream {
                obj.remove("stream_options");
            }
        }
    })
}

fn stream_request_url(url: String, stream: bool) -> String {
    if stream || !url.contains(":streamGenerateContent") {
        return url;
    }
    // Both Gemini and Antigravity use the same RPC suffix. Unary must not
    // retain the SSE query parameter even when the adapter supplied the URL.
    let url = url.replace(":streamGenerateContent", ":generateContent");
    let Some((base, query)) = url.split_once('?') else {
        return url;
    };
    let query = query
        .split('&')
        .filter(|part| *part != "alt=sse")
        .collect::<Vec<_>>()
        .join("&");
    if query.is_empty() {
        base.to_owned()
    } else {
        format!("{base}?{query}")
    }
}

/// Resolves the wire format a provider speaks, deferring to the model row only
/// for `Mixed` providers. Delegates to the shared resolver so the chat and
/// embeddings paths cannot drift apart.
fn resolve_target_format(
    adapter: &openproxy_adapters::adapters::ProviderAdapterEnum,
    model_format: openproxy_types::TargetFormat,
) -> openproxy_types::TargetFormat {
    openproxy_adapters::adapters::resolve_target_format(adapter.format(), model_format)
}

/// Resolves which system prefix to inject for the chosen target.
/// - Checks `target_id` first; if empty and `inherited_parent_id` is present, checks the parent target.
/// - 0 active prefixes: returns None (no prefix injected).
/// - 1 active prefix: returns Some(system_text) directly.
/// - >=2 active prefixes: runs System One decision router (Laya/Jev) evaluating each prefix's `criteria` against the user's prompt to select the best persona.
async fn resolve_system_prefix_for_target(
    ctx: &PipelineContext,
    target_id: openproxy_types::ids::ComboTargetId,
    inherited_parent_id: Option<openproxy_types::ids::ComboTargetId>,
) -> Option<String> {
    let mut prefixes = ctx
        .pipeline
        .read_db(move |conn| {
            openproxy_db::combos::list_active_target_system_prefixes(conn, target_id)
        })
        .await
        .ok()?;

    let effective_target_id = if prefixes.is_empty()
        && let Some(parent_id) = inherited_parent_id
    {
        let parent_prefixes = ctx
            .pipeline
            .read_db(move |conn| {
                openproxy_db::combos::list_active_target_system_prefixes(conn, parent_id)
            })
            .await
            .ok()?;
        if !parent_prefixes.is_empty() {
            tracing::debug!(
                target_id = target_id.0,
                parent_target_id = parent_id.0,
                count = parent_prefixes.len(),
                "inheriting system prefixes from parent sub-combo target"
            );
            prefixes = parent_prefixes;
            parent_id
        } else {
            target_id
        }
    } else {
        target_id
    };

    if prefixes.is_empty() {
        return None;
    }

    if prefixes.len() == 1 {
        tracing::debug!(
            target_id = effective_target_id.0,
            prefix_name = %prefixes[0].name,
            "single active system prefix; injecting directly"
        );
        return Some(prefixes[0].system_text.clone());
    }

    // >= 2 active prefixes: evaluate semantic criteria with System One decision model
    let combo = ctx.combo.as_ref()?;
    let decision_model = combo
        .decision_model
        .clone()
        .unwrap_or_else(|| "jev-latest".to_string());

    let state_text = crate::stages::decision::extract_prompt_state(
        &ctx.req.openai_request,
        crate::stages::decision::MAX_PROMPT_CHARS,
    );

    let mut criteria_map = std::collections::HashMap::new();
    let mut options_vec = Vec::new();
    for p in &prefixes {
        let opt_id = p.id.to_string();
        criteria_map.insert(opt_id.clone(), p.criteria.clone());
        options_vec.push(opt_id);
    }

    let question = openproxy_types::systemone::SystemOneQuestion {
        question_type: openproxy_types::systemone::SystemOneQuestionType::Choice,
        instructions: "Select the most appropriate persona/system prefix ID to handle this user query according to domain, tone, and requirements.".to_string(),
        criteria: serde_json::to_value(&criteria_map).ok(),
        options: Some(options_vec),
    };

    let mut questions = std::collections::BTreeMap::new();
    questions.insert("prefix_selection".to_string(), question);

    let sys_req = openproxy_types::systemone::SystemOneRequest {
        state: serde_json::Value::String(state_text),
        model: Some(decision_model.clone()),
        questions,
    };

    let timeout_ms = combo
        .decision_timeout_ms
        .unwrap_or(crate::stages::decision::DEFAULT_DECISION_TIMEOUT_MS);

    match crate::stages::decision::execute_system_one_decision(
        ctx,
        &decision_model,
        &sys_req,
        timeout_ms,
    )
    .await
    {
        Ok(Some(answer)) => {
            if let Some(ref choice_str) = answer.choice
                && let Ok(chosen_id) = choice_str.parse::<i64>()
                && let Some(found) = prefixes.iter().find(|p| p.id == chosen_id)
            {
                tracing::info!(
                    target_id = target_id.0,
                    chosen_prefix_id = chosen_id,
                    chosen_prefix_name = %found.name,
                    "semantic system prefix router selected persona"
                );
                return Some(found.system_text.clone());
            }
            // Fallback: choose highest priority / first
            Some(prefixes[0].system_text.clone())
        }
        Ok(None) => {
            tracing::debug!(
                target_id = target_id.0,
                "system prefix decision router returned no answer; defaulting to first prefix"
            );
            Some(prefixes[0].system_text.clone())
        }
        Err(e) => {
            tracing::warn!(
                target_id = target_id.0,
                error = %e,
                "system prefix decision router failed; defaulting to first prefix"
            );
            Some(prefixes[0].system_text.clone())
        }
    }
}

pub(crate) fn prepare_messages_for_formatting(
    ctx: &PipelineContext,
) -> &[openproxy_types::OpenAIMessage] {
    let cloned_messages_ref = ctx.req.compressed_messages.get_or_init(|| {
        let pii_enabled = ctx.pipeline.config.pii_config.pii_enabled;
        let compression_needed = openproxy_compression::would_compress(
            &ctx.req.openai_request.messages,
            ctx.pipeline.config.compression_mode,
        );
        // Canonicalise media unconditionally. Every target formatter reads one
        // vocabulary, so a part that keeps the dialect it arrived in (an
        // `input_image` from a Responses client, an Anthropic `document`) is
        // dropped by whichever formatter does not know that spelling. Doing it
        // here — before the formatter is even selected — is what makes the
        // translation matrix work for every client/target pair instead of only
        // for the OpenAI target, whose formatter happens to normalise.
        let media_needs_canonicalisation = ctx
            .req
            .openai_request
            .messages
            .iter()
            .any(|m| matches!(&m.content, Some(serde_json::Value::Array(_))));

        if !pii_enabled && !compression_needed && !media_needs_canonicalisation {
            *ctx.req.compression_stats.lock() =
                Some(openproxy_compression::stats::CompressionStats::empty());
            return None;
        }

        let mut msgs = ctx.req.openai_request.messages.clone();

        if media_needs_canonicalisation {
            for msg in &mut msgs {
                if let Some(serde_json::Value::Array(parts)) = &msg.content {
                    let canonical = crate::formatting::media::canonical_media_parts(parts);
                    msg.content = Some(if canonical.is_empty() {
                        serde_json::Value::String(msg.extract_text())
                    } else {
                        serde_json::Value::Array(canonical)
                    });
                }
            }
        }

        if pii_enabled {
            let engine = crate::pii::PiiEngine::from_config(&ctx.pipeline.config.pii_config);
            let mut session =
                crate::pii::PiiSession::new(ctx.pipeline.config.pii_config.pii_reversible);
            let redacted = engine.redact_messages(&msgs, &mut session);
            msgs = redacted;
            *ctx.req.pii_session.lock() = Some(session);
        }

        if compression_needed {
            let stats = openproxy_compression::apply_compression(
                &mut msgs,
                ctx.pipeline.config.compression_mode,
            );
            *ctx.req.compression_stats.lock() = Some(stats);
        } else {
            *ctx.req.compression_stats.lock() =
                Some(openproxy_compression::stats::CompressionStats::empty());
        }

        Some(msgs)
    });

    cloned_messages_ref
        .as_deref()
        .unwrap_or(&ctx.req.openai_request.messages)
}

#[derive(Clone, Copy)]
pub struct DispatchStage;

impl PipelineStage for DispatchStage {
    async fn execute(
        &self,
        ctx: &mut PipelineContext,
        _next: PipelineNext<'_>,
    ) -> Result<PipelineResult, CoreError> {
        let current = ctx.current_target.as_mut().ok_or_else(|| {
            CoreError::Internal("missing current_target in pipeline context".into())
        })?;
        let cloned_target = current.target.clone();
        let target = &cloned_target;
        let cloned_model = current.model.clone();
        let model = &cloned_model;
        let attempt = ctx.current_target_attempt;
        let race_size = ctx.race_size;
        let started = ctx.started.unwrap_or_else(std::time::Instant::now);
        let trace_id = ctx.trace_id.clone();
        let combo = ctx
            .combo
            .as_ref()
            .ok_or_else(|| CoreError::Internal("missing combo in pipeline context".into()))?;

        let Some(adapter) = crate::context::pinned_adapter(
            &ctx.req,
            &ctx.pipeline.config.adapters,
            &target.provider_id,
        ) else {
            let err = CoreError::ProviderNotFound(target.provider_id.to_string());
            return fail_stage!(ctx, target, &err, Some(model));
        };

        if ctx.race_cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
            return fail_stage!(
                with_trace;
                ctx,
                target,
                &CoreError::RaceLost,
                Some(model),
                CoreError::RaceLost.http_status()
            );
        }

        try_lazy_fetch_antigravity_project(&ctx.pipeline, current).await;

        let api_key = current
            .custom_meta
            .as_ref()
            .map_or(current.api_key.as_str(), |m| m.access_token.as_str());
        let account_label_str = current.api_key_label.as_deref().unwrap_or("");

        let target_format = ctx.target_format.ok_or_else(|| {
            CoreError::Internal("missing target_format in pipeline context".into())
        })?;
        let mut zai_route = if target.provider_id.as_str() == "zai" {
            match super::zai::prepare_zai_route(
                &ctx.pipeline,
                current,
                &adapter.config().base_url,
                ctx.req.proxy_override.as_ref(),
            )
            .await
            {
                Ok(route) => Some(route),
                Err(err) => return fail_stage!(ctx, target, &err, Some(model), err.http_status()),
            }
        } else {
            None
        };
        let url = zai_route.as_ref().map_or_else(
            || {
                adapter.build_chat_url_for_account(
                    target_format,
                    &model.model_id,
                    account_label_str,
                )
            },
            |prepared| prepared.route.url.clone(),
        );
        let url = stream_request_url(url, ctx.is_streaming.unwrap_or(false));
        let effective_key = zai_route
            .as_ref()
            .map_or(api_key, |prepared| prepared.route.credential.as_str());
        let mut headers = adapter.build_headers(effective_key, target_format, &model.model_id);
        let codex_ws = current
            .custom_meta
            .as_ref()
            .and_then(|m| m.codex_workspace_id.as_deref());
        propagate_provider_target_headers(
            &mut headers,
            target.provider_id.as_str(),
            adapter.id().as_str(),
            &ctx.req.request_headers,
            &ctx.req.openai_request,
            codex_ws,
        );

        openproxy_types::emit_stage_event!(
            request_id: ctx.req.request_id,
            trace_id: trace_id,
            stage: "connecting",
            elapsed_ms: started.elapsed().as_millis() as u64,
            api_key_id: ctx.req.api_key_id,
        );

        let body_bytes = ctx
            .body_bytes
            .take()
            .ok_or_else(|| CoreError::Internal("missing body_bytes in pipeline context".into()))?;
        // Finalize only after formatting, wrapping, parameter stripping and
        // header propagation. Signatures and body-derived headers must describe
        // the exact bytes sent to the upstream, independently of the schema.
        let body_bytes = match adapter.finalize_request(
            body_bytes,
            &mut headers,
            target_format,
            &model.model_id,
            current,
        ) {
            Ok(body) => body,
            Err(error) => return fail_stage!(ctx, target, &error, Some(model)),
        };
        let resolved_timeouts = ctx.resolved_timeouts.ok_or_else(|| {
            CoreError::Internal("missing resolved_timeouts in pipeline context".into())
        })?;

        let mut result = ctx
            .pipeline
            .dispatcher
            .dispatch_upstream(crate::upstream_dispatcher::DispatchParams {
                target,
                combo,
                req: ctx.req.clone(),
                model,
                target_format,
                url: &url,
                headers: &headers,
                body_bytes: body_bytes.clone(),
                resolved_timeouts: &resolved_timeouts,
                started,
                attempt,
                race_size,
                trace_id,
                is_streaming: ctx.is_streaming.unwrap_or(false),
            })
            .await;

        let starter_depleted = zai_route.as_ref().is_some_and(|prepared| {
            prepared.route.source == openproxy_types::quota::QuotaSource::AutoclawStarter
                || prepared.route.source == openproxy_types::quota::QuotaSource::ZcodeStarter
        }) && result
            .error
            .as_ref()
            .is_some_and(openproxy_adapters::adapters::zai::is_zcode_entitlement_exhaustion);
        if starter_depleted
            && !ctx.is_streaming.unwrap_or(false)
            && ctx.req.stream_sink.is_none()
            && let Some(prepared) = zai_route.as_mut()
            && super::zai::prepare_zai_paid_fallback(
                &ctx.pipeline,
                prepared,
                model.model_id.as_str(),
                &adapter.config().base_url,
            )
            .await
            .is_ok()
            && !ctx
                .race_cancel
                .as_ref()
                .is_some_and(|cancel| cancel.is_cancelled())
        {
            let mut paid_headers =
                adapter.build_headers(&prepared.route.credential, target_format, &model.model_id);
            propagate_provider_target_headers(
                &mut paid_headers,
                target.provider_id.as_str(),
                adapter.id().as_str(),
                &ctx.req.request_headers,
                &ctx.req.openai_request,
                codex_ws,
            );
            let body_bytes = match adapter.finalize_request(
                body_bytes,
                &mut paid_headers,
                target_format,
                &model.model_id,
                current,
            ) {
                Ok(body) => body,
                Err(error) => return fail_stage!(ctx, target, &error, Some(model)),
            };
            result = ctx
                .pipeline
                .dispatcher
                .dispatch_upstream(crate::upstream_dispatcher::DispatchParams {
                    target,
                    combo,
                    req: ctx.req.clone(),
                    model,
                    target_format,
                    url: &prepared.route.url,
                    headers: &paid_headers,
                    body_bytes,
                    resolved_timeouts: &resolved_timeouts,
                    started,
                    attempt,
                    race_size,
                    trace_id: ctx.trace_id.clone(),
                    is_streaming: ctx.is_streaming.unwrap_or(false),
                })
                .await;
        }

        if let Some(prepared) = zai_route.as_mut()
            && let Some(error) = result.error.as_ref()
            && let Err(error) = super::zai::record_zai_route_failure(
                &ctx.pipeline,
                prepared,
                model.model_id.as_str(),
                error,
            )
            .await
        {
            return fail_stage!(ctx, target, &error, Some(model), error.http_status());
        }
        update_circuit_breaker_on_result(&ctx.pipeline, target, model, &result);
        update_predictive_limiter_on_result(&ctx.pipeline, target, &result);
        update_account_rate_limited_until_on_result(&ctx.pipeline, target, &result);

        Ok(result)
    }
}

async fn try_lazy_fetch_antigravity_project(
    pipeline: &crate::Pipeline,
    current: &mut crate::context::ResolvedTarget,
) {
    if current.target.provider_id.as_str() != "antigravity" {
        return;
    }
    let Some(custom_meta) = current.custom_meta.as_mut() else {
        return;
    };
    if custom_meta.antigravity_project.is_some() {
        return;
    }
    let Some(ref meta_str) = custom_meta.antigravity_metadata else {
        return;
    };
    let Ok(metadata) = serde_json::from_str::<serde_json::Value>(meta_str) else {
        return;
    };

    tracing::info!(
        "Lazy fetching antigravity projectId for target {}",
        current.target.id.0
    );
    match openproxy_adapters::adapters::antigravity::load_code_assist(
        &pipeline.config.upstream_client,
        &custom_meta.access_token,
        &metadata,
    )
    .await
    {
        Ok(Some(pid)) => {
            tracing::info!("Successfully fetched antigravity projectId: {}", pid);
            custom_meta.antigravity_project = Some(pid.clone());
            if let Some(account_id) = current.target.account_id {
                let result = pipeline
                    .async_repo()
                    .run(move |repo| repo.update_antigravity_project_id(account_id.0, &pid))
                    .await;
                if let Err(e) = result {
                    tracing::error!("Failed to update antigravity project id in db: {}", e);
                }
            }
        }
        Ok(None) => tracing::warn!("loadCodeAssist returned Ok(None)"),
        Err(e) => tracing::error!("loadCodeAssist failed: {}", e),
    }
}

pub(crate) use super::target_breaker::*;

#[derive(Clone, Copy)]
pub struct CustomAdapterStage;

impl PipelineStage for CustomAdapterStage {
    async fn execute(
        &self,
        ctx: &mut PipelineContext,
        next: PipelineNext<'_>,
    ) -> Result<PipelineResult, CoreError> {
        let Some(current) = ctx.current_target.as_mut() else {
            return Err(CoreError::Internal(
                "missing current_target in pipeline context".into(),
            ));
        };
        let cloned_target = current.target.clone();
        let target = &cloned_target;
        let Some(_adapter) = crate::context::pinned_adapter(
            &ctx.req,
            &ctx.pipeline.config.adapters,
            &target.provider_id,
        ) else {
            let err = CoreError::ProviderNotFound(target.provider_id.to_string());
            return fail_stage!(with_trace; ctx, target, &err, Some(&current.model), 0);
        };

        next.execute(ctx).await
    }
}

use super::target_headers::propagate_provider_target_headers;

#[cfg(test)]
#[path = "stream_policy_tests.rs"]
mod stream_policy_tests;

#[cfg(test)]
#[path = "pin_tests.rs"]
mod pin_tests;

#[cfg(test)]
#[path = "target_tests.rs"]
mod tests;
