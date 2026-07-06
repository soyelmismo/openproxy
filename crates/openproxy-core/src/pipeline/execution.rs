use crate::adapters::{AdapterFormat, ProviderAdapter};
use crate::combos::{self, Combo, ComboTarget};
use crate::compression::{CompressionMode, stats::CompressionStats};
use crate::error::{CoreError, Result};
use crate::cost::UsageInput;
use crate::ids::ComboId;
use crate::models::Model;
use crate::pipeline::{FailureContext, ErrorPhase, Pipeline, PipelineRequest, PipelineResult, is_upstream_health_issue};
use crate::retry::RetryPolicy;
use crate::timeouts::{self, ModelTimeoutOverrides};
use crate::upstream::CancellationToken;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::watch;
use crate::pipeline::repository::PipelineRepository;

impl Pipeline {
    /// Drive one chat-completion request to completion.
    pub async fn run(&self, req: Arc<PipelineRequest>) -> PipelineResult {
        use crate::pipeline::stage::PipelineChain;
        use crate::pipeline::context::PipelineContext;
        use crate::pipeline::stages::{
            telemetry::TelemetryRecorderStage,
            router::RouterStage,
            quota::QuotaEnforcerStage,
            executor::UpstreamExecutorStage,
        };

        let ctx = PipelineContext::new(req, self.clone());
        let chain = PipelineChain::new(vec![
            Arc::new(TelemetryRecorderStage),
            Arc::new(RouterStage),
            Arc::new(QuotaEnforcerStage),
            Arc::new(UpstreamExecutorStage),
        ]);

        match chain.execute(ctx).await {
            Ok(result) => result,
            Err(e) => {
                // Fallback if the chain entirely fails without catching
                self.failure(e, 1, ErrorPhase::Route)
            }
        }
    }

    pub(crate) fn flatten_targets(
        &self,
        root_combo_id: &ComboId,
        targets: Vec<ComboTarget>,
    ) -> Result<Vec<ComboTarget>> {
        if !targets.iter().any(|t| t.sub_combo_id.is_some()) {
            return Ok(targets);
        }
        let mut out = Vec::with_capacity(targets.len());
        let conn = self.conn.lock();
        let mut visited: Vec<ComboId> = vec![*root_combo_id];
        for t in targets {
            if let Some(sub_id) = t.sub_combo_id {
                let sub_flat = combos::resolve_combo_to_targets(&conn, sub_id, &mut visited, 0)?;
                out.extend(sub_flat);
            } else {
                out.push(t);
            }
        }
        let expanded = combos::expand_account_rotation(&conn, out)?;
        Ok(expanded)
    }

    pub(crate) fn auto_populate_if_empty(&self, combo: &Combo) -> Result<usize> {
        {
            let conn = self.conn.lock();
            if !combos::list_targets(&conn, combo.id)?.is_empty() {
                return Ok(0);
            }
        }

        let added = {
            let conn = self.conn.lock();
            combos::auto_populate_empty_combo(&conn, combo.id)?
        };

        if added > 0 {
            tracing::info!(
                combo_id = combo.id.0,
                combo_name = %combo.name,
                added_targets = added,
                "auto-populated empty combo with healthy provider's active models"
            );
        }
        Ok(added)
    }

    pub(crate) fn record_no_healthy_targets_row(
        &self,
        req: Arc<PipelineRequest>,
        combo: &Combo,
        started: Instant,
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



    pub async fn resolve_combo_targets_full(&self, eligible: Vec<ComboTarget>) -> Vec<crate::pipeline::context::ResolvedTarget> {
        if eligible.is_empty() {
            return Vec::new();
        }

        let mut model_row_ids = Vec::new();
        let mut account_ids = Vec::new();
        let mut provider_ids_no_account = Vec::new();

        for t in &eligible {
            if let Some(m) = t.model_row_id {
                model_row_ids.push(m);
            }
            if let Some(a) = t.account_id {
                account_ids.push(a);
            } else {
                provider_ids_no_account.push(t.provider_id.clone());
            }
        }

        model_row_ids.sort_unstable_by_key(|id| id.0);
        model_row_ids.dedup_by_key(|id| id.0);
        account_ids.sort_unstable_by_key(|id| id.0);
        account_ids.dedup_by_key(|id| id.0);
        provider_ids_no_account.sort_unstable_by_key(|id| id.0.clone());
        provider_ids_no_account.dedup_by_key(|id| id.0.clone());

        let models_map = self.repo().get_models_by_row_ids(&model_row_ids).unwrap_or_default();
        let (accounts_map, kiro_map, antigravity_map) = self.repo().get_accounts_meta(&account_ids).unwrap_or_default();
        let providers_map = self.repo().get_providers_auth_type(&provider_ids_no_account).unwrap_or_default();

        let master_key = self.config.master_key.clone();
        let oauth_registry = self.config.oauth_provider_registry.clone();

        tokio::task::spawn_blocking(move || {
            crate::pipeline::credentials::CredentialManager::resolve_credentials(
                eligible,
                models_map,
                accounts_map,
                kiro_map,
                antigravity_map,
                providers_map,
                master_key,
                oauth_registry,
            )
        }).await.unwrap()
    }

    pub(crate) async fn execute_single(
        &self,
        req: Arc<PipelineRequest>,
        combo: &Combo,
        resolved_target: &crate::pipeline::context::ResolvedTarget,
        attempt: u8,
        race_size: u8,
        race_cancel: &CancellationToken,
    ) -> PipelineResult {
        let target = &resolved_target.target;
        let model = &resolved_target.model;
        let started = Instant::now();
        let trace_id = if attempt > 1 {
            format!("{}:retry{}", req.trace_id, attempt - 1)
        } else {
            req.trace_id.to_string()
        };

        if race_cancel.is_cancelled() {
            return self.record_and_fail_with_trace_id(
                req,
                combo,
                target,
                FailureContext {
                    attempt,
                    race_size,
                    err: &CoreError::RaceLost,
                    started,
                    model: None,
                    connect_ms: None,
                    ttft_ms: None,
                    status_code: CoreError::RaceLost.http_status(),
                },
                trace_id,
            );
        }

        crate::usage::publish_stage_event(crate::usage::StageEvent {
            request_id: req.request_id.to_string(),
            trace_id: trace_id.to_string(),
            provider_id: target.provider_id.to_string(),
            upstream_model_id: model.model_id.as_str().to_string(),
            stage: "started".into(),
            elapsed_ms: 0,
            connect_ms: None,
            ttft_ms: None,
            status_code: 0,
            error: None,
            stop_reason: None,
            compression_savings_pct: None,
            compression_techniques: None,
            timestamp: String::new(),
            endpoint_kind: crate::endpoint::EndpointKind::Chat,
        });

        let adapter = match self.adapter_for(&target.provider_id) {
            Some(a) => a,
            None => {
                let err = CoreError::ProviderNotFound(target.provider_id.to_string());
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    FailureContext {
                        attempt,
                        race_size,
                        err: &err,
                        started,
                        model: None,
                        connect_ms: None,
                        ttft_ms: None,
                        status_code: 0,
                    },
                );
            }
        };

        let mut resolved_target_clone = resolved_target.clone();
        
        if let Some(account_id) = target.account_id {
            if let Some(custom_meta) = &mut resolved_target_clone.custom_meta {
                if let Some(refresh_token) = &custom_meta.maybe_refresh {
                    if let Some(registry) = self.config.oauth_provider_registry.as_ref() {
                        if let Some(provider) = registry.get(target.provider_id.as_str()) {
                            let provider_id_str = target.provider_id.as_str();
                            tracing::info!(account = account_id.0, provider = provider_id_str, "pipeline: proactive OAuth token refresh");
                            match provider.refresh_token(refresh_token, &self.config.upstream_client, account_id, crate::oauth::DbRef::Connection(&self.conn)).await {
                                Ok(token) => {
                                    let expires_at = token.expires_in.map(|secs| {
                                        (chrono::Utc::now() + chrono::Duration::seconds(secs as i64)).format("%Y-%m-%dT%H:%M:%SZ").to_string()
                                    });
                                    {
                                        let conn = self.conn.lock();
                                        let _ = crate::accounts::store_oauth_tokens(&conn, account_id, &token.access_token, token.refresh_token.as_deref(), &self.config.master_key, &token.token_type, expires_at.as_deref(), token.scope.as_deref(), None, None);
                                    }
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

        if let Some(result) = adapter.execute_custom(&self.config.upstream_client, Arc::clone(&req), &resolved_target_clone).await {
            return match result {
                Ok(response) => {
                    let total_ms = started.elapsed().as_millis() as u64;
                    let usage_tuple = match self.record_attempt_raw_with_tokens(
                        Arc::clone(&req), combo, target, Some(&model), None, None, None, total_ms, 200, attempt, race_size, trace_id.clone(),
                        response.usage.as_ref().map(|u| u.prompt_tokens),
                        response.usage.as_ref().map(|u| u.completion_tokens),
                        None, None, None, None, false, true, None,
                    ) {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::warn!(error = %e, "record_attempt_raw_with_tokens failed; non-fatal");
                            None
                        }
                    };
                    PipelineResult {
                        status_code: 200,
                        error: None,
                        final_response: Some(response),
                        attempts: attempt,
                        usage_tuple,
                    }
                }
                Err(e) => {
                    if let CoreError::UpstreamError { status: 401, .. } = &e {
                        if let Some(account_id) = target.account_id {
                            let provider_id_str = target.provider_id.to_string();
                            let dedup_key = format!("{}:{}", crate::notifications::CODE_OAUTH_EXPIRED, account_id.0);
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
                            let conn = self.conn.lock();
                            let _ = crate::notifications::insert_and_broadcast(&conn, crate::notifications::KIND_SYSTEM, &payload, Some(&dedup_key), Some(&provider_id_str));
                        }
                    }
                    self.record_and_fail_with_trace_id(
                        req, combo, target, FailureContext {
                            attempt, race_size, err: &e, started, model: Some(&model), connect_ms: None, ttft_ms: None, status_code: e.http_status(),
                        }, trace_id
                    )
                }
            };
        }

        let resolved_timeouts = {
            let model_overrides =
                match ModelTimeoutOverrides::from_json(model.timeout_overrides_json.as_deref()) {
                    Ok(o) => o,
                    Err(e) => {
                        return self.record_and_fail(
                            req,
                            combo,
                            target,
                            FailureContext {
                                attempt,
                                race_size,
                                err: &e,
                                started,
                                model: Some(&model),
                                connect_ms: None,
                                ttft_ms: None,
                                status_code: 0,
                            },
                        );
                    }
                };
            timeouts::resolve(&self.config.defaults, Some(&model_overrides))
        };
        tracing::debug!(
            combo_id = combo.id.0,
            target_id = target.id.0,
            provider = %target.provider_id,
            model = %model.model_id.as_str(),
            connect_ms = resolved_timeouts.connect.as_millis() as u64,
            request_send_ms = resolved_timeouts.request_send.as_millis() as u64,
            ttft_ms = resolved_timeouts.ttft.as_millis() as u64,
            idle_chunk_ms = resolved_timeouts.idle_chunk.as_millis() as u64,
            total_ms = resolved_timeouts.total.as_millis() as u64,
            "resolved timeouts for target"
        );

        let target_format = match adapter.format() {
            AdapterFormat::Openai => crate::models::TargetFormat::Openai,
            AdapterFormat::Anthropic => crate::models::TargetFormat::Anthropic,
            AdapterFormat::Mixed => model.target_format,
            AdapterFormat::Gemini => crate::models::TargetFormat::Gemini,
        };

        let stream = if !req.openai_request.stream && req.stream_sink.is_some() {
            true
        } else {
            req.openai_request.stream
        };

        // Deep clone messages ONLY if we actually need compression
        let mut cloned_messages: Option<Vec<crate::translation::OpenAIMessage>> = None;
        let compression_stats = if self.config.compression_mode != CompressionMode::Off {
            let mut msgs = req.openai_request.messages.clone();
            let stats = crate::compression::apply_compression(
                &mut msgs,
                self.config.compression_mode,
            );
            cloned_messages = Some(msgs);
            stats
        } else {
            CompressionStats::empty()
        };
        *self.compression_stats_cell.write() = Some(compression_stats);

        let messages_ref = cloned_messages.as_deref().unwrap_or(&req.openai_request.messages);

        let formatter = crate::pipeline::formatting::get_formatter(target_format);
        let body_bytes = match formatter.format_request(&req, &model, messages_ref, stream, adapter.as_ref()) {
            Ok(b) => b,
            Err(e) => {
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    FailureContext {
                        attempt,
                        race_size,
                        err: &e,
                        started,
                        model: Some(&model),
                        connect_ms: None,
                        ttft_ms: None,
                        status_code: 0,
                    },
                );
            }
        };

        let (api_key, account_label) = match self.resolve_target_api_key_and_label(target) {
            Ok(v) => v,
            Err(e) => {
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    FailureContext {
                        attempt,
                        race_size,
                        err: &e,
                        started,
                        model: Some(&model),
                        connect_ms: None,
                        ttft_ms: None,
                        status_code: 0,
                    },
                );
            }
        };

        let account_label_str = account_label.as_deref().unwrap_or("");
        let url =
            adapter.build_chat_url_for_account(target_format, &model.model_id, account_label_str);
        let headers = adapter.build_headers(&api_key, target_format, &model.model_id);

        if race_cancel.is_cancelled() {
            return self.record_and_fail_with_trace_id(
                req,
                combo,
                target,
                FailureContext {
                    attempt,
                    race_size,
                    err: &CoreError::RaceLost,
                    started,
                    model: Some(&model),
                    connect_ms: None,
                    ttft_ms: None,
                    status_code: CoreError::RaceLost.http_status(),
                },
                trace_id,
            );
        }

        let compression_stats_at_connecting = self.compression_stats_cell.read().clone();
        crate::usage::publish_stage_event(crate::usage::StageEvent {
            request_id: req.request_id.to_string(),
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

        let result = self
            .dispatch_upstream(
                target,
                combo,
                Arc::clone(&req),
                &model,
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
                Some(e) if RetryPolicy::is_retryable(e, self.config.idle_chunk_retryable) => {
                    let outcome = self.circuit_breaker.record_failure_outcome(aid);
                    if outcome.just_opened {
                        let provider_id_str = target.provider_id.to_string();
                        let model_id_str = model.model_id.as_str().to_string();
                        let combo_target_id = target.id.0;
                        let conn = self.conn.lock();
                        let dedup_key =
                            format!("{}:{}", crate::notifications::CODE_CIRCUIT_OPEN, aid.0);
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
                        let _ = crate::notifications::insert_and_broadcast(
                            &conn,
                            crate::notifications::KIND_SYSTEM,
                            &payload,
                            Some(&dedup_key),
                            Some(&provider_id_str),
                        );
                    }
                }
                _ => {
                    self.circuit_breaker.record_success(aid);
                }
            }
        }

        result
    }

    pub(crate) fn load_combo(&self, req: &PipelineRequest) -> Result<Combo> {
        if let Some(combo) = req.combo_override.as_ref() {
            return Ok(combo.clone());
        }
        let conn = self.conn.lock();
        combos::get_combo(&conn, req.combo_id)?.ok_or(CoreError::ComboNotFound(req.combo_id.0))
    }

    pub(crate) fn resolve_targets(
        &self,
        combo: &Combo,
        targets_override: Option<&[ComboTarget]>,
    ) -> Result<Vec<ComboTarget>> {
        if let Some(overrides) = targets_override {
            let conn = self.conn.lock();
            return combos::expand_account_rotation(&conn, overrides.to_vec());
        }

        let conn = self.conn.lock();
        let _ = combos::list_targets(&conn, combo.id)?;
        let ordered = combos::resolve_target_order_with_mode(
            &conn,
            combo,
            &self.rr_counters,
            &self.selection_registry,
        )?;
        combos::expand_account_rotation(&conn, ordered)
    }

    fn adapter_for(
        &self,
        provider_id: &crate::ids::ProviderId,
    ) -> Option<Arc<dyn ProviderAdapter>> {
        self.config
            .adapters
            .iter()
            .find(|a| a.id() == provider_id)
            .cloned()
    }

    #[cfg(test)]
    pub(crate) fn decrypt_account_key(&self, account_id: crate::ids::AccountId) -> Result<String> {
        let conn = self.conn.lock();
        crate::accounts::decrypt_api_key(&conn, account_id, &self.config.master_key)
    }

    #[cfg(test)]
    pub(crate) fn resolve_target_api_key(&self, target: &ComboTarget) -> Result<String> {
        match target.account_id {
            Some(account_id) => self.decrypt_account_key(account_id),
            None => {
                let conn = self.conn.lock();
                match crate::providers::get(&conn, &target.provider_id)? {
                    Some(p) if matches!(p.auth_type, crate::providers::AuthType::None) => {
                        Ok(String::new())
                    }
                    Some(p) if p.id.0 == "opencode-zen" => {
                        Ok(String::new())
                    }
                    _ => Err(CoreError::Auth(format!(
                        "combo_target {} has no account_id after expansion",
                        target.id.0
                    ))),
                }
            }
        }
    }

    fn resolve_target_api_key_and_label(
        &self,
        target: &ComboTarget,
    ) -> Result<(String, Option<String>)> {
        match target.account_id {
            Some(account_id) => {
                let conn = self.conn.lock();
                crate::accounts::decrypt_api_key_and_label(
                    &conn,
                    account_id,
                    &self.config.master_key,
                )
            }
            None => {
                let conn = self.conn.lock();
                match crate::providers::get(&conn, &target.provider_id)? {
                    Some(p) if matches!(p.auth_type, crate::providers::AuthType::None) => {
                        Ok((String::new(), None))
                    }
                    Some(p) if p.id.0 == "opencode-zen" => {
                        Ok((String::new(), None))
                    }
                    _ => Err(CoreError::Auth(format!(
                        "combo_target {} has no account_id after expansion",
                        target.id.0
                    ))),
                }
            }
        }
    }
    pub(crate) fn mark_client_response(&self, usage_tuple: Option<(String, u8, crate::ids::ComboTargetId)>) {
        let Some((request_id, attempt, target_id)) = usage_tuple else { return };
        let job = crate::pipeline::worker::BackgroundJob::MarkClientResponse { request_id, attempt, target_id };
        if let Err(e) = self.config.background_tx.try_send(job) {
            if matches!(e, tokio::sync::mpsc::error::TrySendError::Closed(_)) {
                let job = e.into_inner();
                let conn = self.conn.clone();
                crate::pipeline::worker::process_job(&conn, job);
            } else {
                tracing::warn!("failed to send RecordAttempt to background worker: {}", e);
            }
        }
    }

    pub(crate) fn failure(&self, err: CoreError, attempts: u8, _phase: ErrorPhase) -> PipelineResult {
        PipelineResult {
            status_code: err.http_status(),
            error: Some(err),
            final_response: None,
            attempts,
            usage_tuple: None,
        }
    }

    pub(crate) fn client_disconnected_result(&self, attempts: u8) -> PipelineResult {
        self.failure(CoreError::ClientDisconnected, attempts, ErrorPhase::Retry)
    }

    pub(crate) fn is_client_disconnected(&self, rx: &mut watch::Receiver<bool>) -> bool {
        *rx.borrow_and_update()
    }

    pub(crate) fn check_and_trigger_proxy_rotation(
        &self,
        provider_id: &crate::ids::ProviderId,
        status_code: Option<u16>,
        is_connect_error: bool,
    ) {
        let conn = self.conn.lock();
        if let Ok(Some(provider)) = crate::providers::get(&conn, provider_id)
            && provider.use_proxies {
                let mut should_rotate = false;
                let errors_list: Vec<&str> = provider.proxy_rotation_errors
                    .split(',')
                    .map(|s| s.trim())
                    .collect();

                if let Some(sc) = status_code {
                    let sc_str = sc.to_string();
                    if errors_list.contains(&sc_str.as_str()) {
                        should_rotate = true;
                    }
                }

                if is_connect_error
                    && (errors_list.contains(&"connect_error") || errors_list.contains(&"timeout")) {
                        should_rotate = true;
                    }

                if should_rotate
                    && let Some(ref bad_proxy_id) = provider.current_proxy_id {
                        tracing::warn!(
                            provider = %provider_id,
                            proxy_id = %bad_proxy_id,
                            "proxy rotation triggered: marking proxy as dead and clearing binding"
                        );
                        let _ = crate::free_proxies::update_proxy_status(&conn, bad_proxy_id, "dead", None);
                        let _ = crate::providers::update_current_proxy(&conn, provider_id, None);
                    }
            }
    }

    pub(crate) fn record_and_fail(
        &self,
        req: Arc<PipelineRequest>,
        combo: &Combo,
        target: &ComboTarget,
        ctx: FailureContext<'_>,
    ) -> PipelineResult {
        self.record_and_fail_with_trace_id(Arc::clone(&req), combo, target, ctx, req.trace_id.to_string())
    }

    pub(crate) fn record_and_fail_with_trace_id(
        &self,
        req: Arc<PipelineRequest>,
        combo: &Combo,
        target: &ComboTarget,
        ctx: FailureContext<'_>,
        trace_id: String,
    ) -> PipelineResult {
        self.record_and_fail_with_trace_id_and_partial(
            req, combo, target, ctx, trace_id, None, None, 0, "",
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_and_fail_with_trace_id_and_partial(
        &self,
        req: Arc<PipelineRequest>,
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
        let request_body_json = req
            .request_body_json
            .clone()
            .or_else(|| serde_json::to_value(&*req.openai_request).ok().map(Arc::new));
        let request_headers = crate::redact::redact_btreemap_sensitive(req.request_headers.clone());
        let response_body_json: Option<serde_json::Value> =
            acc.filter(|a| !a.is_completely_empty()).map(|a| {
                let chunk_id_str = chunk_id.unwrap_or("partial");
                a.finish(chunk_id_str, created, model_name)
            });
        let was_streaming = req.stream_sink.is_some();
        let (is_streaming, stream_complete) = if response_body_json.is_some() {
            (true, false)
        } else {
            (was_streaming, false)
        };
        let usage_tuple = match self.record_attempt_raw_with_tokens(
            Arc::clone(&req),
            combo,
            target,
            model,
            Some(err),
            connect_ms,
            ttft_ms,
            total_ms,
            status_code,
            attempt,
            race_size,
            trace_id,
            None,
            None,
            request_body_json.clone(),
            response_body_json,
            Some(request_headers),
            None,
            is_streaming,
            stream_complete,
            None,
        ) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(error = %e, "record_attempt_raw_with_tokens failed; non-fatal");
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

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn fail_stream_client_disconnected(
        &self,
        req: Arc<PipelineRequest>,
        combo: &Combo,
        target: &ComboTarget,
        attempt: u8,
        race_size: u8,
        started: Instant,
        model: &Model,
        connect_ms: u64,
        ttft_ms: Option<u64>,
        trace_id: String,
        acc: Option<&mut crate::sse_accumulator::ResponseAccumulator>,
        chunk_id: &str,
        created: u64,
        model_name: &str,
    ) -> PipelineResult {
        let has_partial_content = acc
            .as_ref()
            .is_some_and(|a| !a.is_empty());
        if let Some(ref a) = acc
            && let Some((code, message)) = a.extract_upstream_error_from_raw() {
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
                };
                let acc_ref: Option<&crate::sse_accumulator::ResponseAccumulator> = match acc {
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
                    FailureContext {
                        attempt,
                        race_size,
                        err: &err,
                        started,
                        model: Some(model),
                        connect_ms: Some(connect_ms),
                        ttft_ms,
                        status_code: code,
                    },
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
                "stream interrupted — client disconnected after receiving partial content".into()
            )
        } else {
            CoreError::ClientDisconnected
        };
        self.record_and_fail_with_trace_id_and_partial(
            req,
            combo,
            target,
            FailureContext {
                attempt,
                race_size,
                err: &err,
                started,
                model: Some(model),
                connect_ms: Some(connect_ms),
                ttft_ms,
                status_code: 499,
            },
            trace_id,
            acc_ref,
            Some(chunk_id),
            created,
            model_name,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn fail_on_sink_send_error(
        &self,
        e: crate::race_sink::StreamSinkError,
        req: Arc<PipelineRequest>,
        combo: &Combo,
        target: &ComboTarget,
        attempt: u8,
        race_size: u8,
        started: Instant,
        model: &Model,
        connect_ms: u64,
        ttft_ms: Option<u64>,
        trace_id: String,
        acc: Option<&mut crate::sse_accumulator::ResponseAccumulator>,
        chunk_id: &str,
        created: u64,
        model_name: &str,
    ) -> PipelineResult {
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
                    && let Some((code, message)) = a.extract_upstream_error_from_raw() {
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
                            };
                            let acc_ref: Option<&crate::sse_accumulator::ResponseAccumulator> =
                                match acc {
                                    Some(a) => {
                                        a.mark_partial();
                                        Some(&*a)
                                    }
                                    None => None,
                                };
                            self.record_and_fail_with_trace_id_and_partial(
                                req,
                                combo,
                                target,
                                FailureContext {
                                    attempt,
                                    race_size,
                                    err: &err,
                                    started,
                                    model: Some(model),
                                    connect_ms: Some(connect_ms),
                                    ttft_ms: None,
                                    status_code: code,
                                },
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
        self.record_and_fail_with_trace_id_and_partial(
            req,
            combo,
            target,
            FailureContext {
                attempt,
                race_size,
                err: &err,
                started,
                model: Some(model),
                connect_ms: Some(connect_ms),
                ttft_ms,
                status_code: err.http_status(),
            },
            trace_id,
            acc_ref,
            Some(chunk_id),
            created,
            model_name,
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_attempt_raw_with_tokens(
        &self,
        req: Arc<PipelineRequest>,
        combo: &Combo,
        target: &ComboTarget,
        model: Option<&Model>,
        err: Option<&CoreError>,
        connect_ms: Option<u64>,
        ttft_ms: Option<u64>,
        total_ms: u64,
        status_code: u16,
        attempt: u8,
        race_size: u8,
        trace_id: String,
        prompt_tokens: Option<u32>,
        completion_tokens: Option<u32>,
        request_body_json: Option<Arc<serde_json::Value>>,
        response_body_json: Option<serde_json::Value>,
        request_headers: Option<std::collections::BTreeMap<String, String>>,
        response_headers: Option<std::collections::BTreeMap<String, String>>,
        is_streaming: bool,
        stream_complete: bool,
        stop_reason: Option<String>,
    ) -> Result<Option<(String, u8, crate::ids::ComboTargetId)>> {
        let recording = self.is_recording();
        let compression_stats_snapshot = self.compression_stats_cell.read().clone();
        let compression_savings_pct = compression_stats_snapshot
            .as_ref()
            .and_then(|s| s.savings_pct_opt());
        let compression_techniques = compression_stats_snapshot
            .as_ref()
            .and_then(|s| s.techniques_csv());

        let (prompt_tokens, prompt_tokens_estimated) = match prompt_tokens {
            Some(t) if t > 0 => (Some(t), false),
            _ => {
                let est = crate::token_estimate::estimate_prompt_tokens(&req.openai_request.messages);
                if est > 0 {
                    tracing::debug!(
                        request_id = %req.request_id,
                        estimated_prompt_tokens = est,
                        "upstream did not report usage; estimated prompt tokens from request messages"
                    );
                    (Some(est), true)
                } else {
                    (None, false)
                }
            }
        };

        let (completion_tokens, completion_tokens_estimated) = match completion_tokens {
            Some(t) if t > 0 => (Some(t), false),
            _ => {
                let completion_text = response_body_json
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
                        request_id = %req.request_id,
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
            request_id: req.request_id,
            trace_id: trace_id.clone(),
            attempt,
            provider_id: target.provider_id.clone(),
            account_id: target.account_id,
            combo_id: Some(combo.id),
            combo_target_id: Some(target.id),
            model_row_id: model.map(|m| m.row_id),
            upstream_model_id: model
                .map(|m| m.model_id.as_str().to_string())
                .unwrap_or_default(),
            prompt_tokens,
            completion_tokens,
            connect_ms,
            ttft_ms,
            total_ms,
            status_code,
            error_msg: err.map(|e| format!("{}", e)),
            race_total: race_size,
            race_lost: err.is_some() && req.race_cancelled,
            api_key_id: req.api_key_id,
            request_body_json: if recording { request_body_json.map(|v| (*v).clone()) } else { None },
            response_body_json: if recording { response_body_json } else { None },
            request_headers: if recording { request_headers } else { None },
            response_headers: if recording { response_headers } else { None },
            error_message: err.map(|e| format!("{}", e)),
            race_attempts: race_size,
            is_streaming,
            stream_complete,
            stop_reason: stop_reason.clone(),
            compression_savings_pct,
            compression_techniques,
            client_response: false,
            prompt_tokens_estimated,
            completion_tokens_estimated,
            endpoint_kind: crate::endpoint::EndpointKind::Chat,
        };

        {
            let stage_label: &str = if err.is_none() {
                "completed"
            } else if req.race_cancelled {
                "cancelled"
            } else {
                "failed"
            };
            let error_str: Option<String> =
                err.map(|e| crate::cost::redact_error_msg(&e.to_string()).0);
            let terminal_snapshot = self.compression_stats_cell.read().clone();
            crate::usage::publish_stage_event(crate::usage::StageEvent {
                request_id: req.request_id.to_string(),
                trace_id: trace_id.to_string(),
                provider_id: target.provider_id.to_string(),
                upstream_model_id: model
                    .map(|m| m.model_id.as_str().to_string())
                    .unwrap_or_default(),
                stage: stage_label.into(),
                elapsed_ms: total_ms,
                connect_ms,
                ttft_ms,
                status_code,
                error: error_str,
                stop_reason: stop_reason.clone(),
                compression_savings_pct: terminal_snapshot
                    .as_ref()
                    .and_then(|s| s.savings_pct_opt()),
                compression_techniques: terminal_snapshot.as_ref().and_then(|s| s.techniques_csv()),
                timestamp: String::new(),
                endpoint_kind: crate::endpoint::EndpointKind::Chat,
            });
        }

        let err_msg = err.map(|e| e.to_string());
        let is_health_issue = err.is_some_and(|e| is_upstream_health_issue(e));

        let job = crate::pipeline::worker::BackgroundJob::RecordAttempt {
            usage_input: input,
            target_id: target.id,
            combo_id: combo.id,
            error_msg: err_msg,
            is_upstream_health_issue: is_health_issue,
            cooldown_mode: combo.cooldown_mode,
            cooldown_base_secs: combo.cooldown_base_secs.unwrap_or(self.config.cooldown_secs),
            cooldown_max_secs: combo.cooldown_max_secs.unwrap_or(self.config.cooldown_max_secs),
            cooldown_factor: combo.cooldown_factor.unwrap_or(self.config.cooldown_factor),
        };

        if let Err(e) = self.config.background_tx.try_send(job) {
            if matches!(e, tokio::sync::mpsc::error::TrySendError::Closed(_)) {
                let job = e.into_inner();
                let conn = self.conn.clone();
                crate::pipeline::worker::process_job(&conn, job);
            } else {
                tracing::warn!("failed to send MarkClientResponse to background worker: {}", e);
            }
        }

        self.selection_registry.record_request(target.id);
        if err.is_none() {
            self.selection_registry.record_success(target.id);
        }

        Ok(Some((req.request_id.to_string(), attempt, target.id)))
    }
}
