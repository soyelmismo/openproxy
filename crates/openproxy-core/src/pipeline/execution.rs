use crate::combos::{self, Combo, ComboTarget};
use crate::error::{CoreError, Result};
use crate::ids::ComboId;
use crate::pipeline::repository::PipelineRepository;
use crate::pipeline::{ErrorPhase, FailureContext, Pipeline, PipelineRequest, PipelineResult};
use crate::upstream::CancellationToken;
use tokio::sync::watch;

impl Pipeline {
    /// Drive one chat-completion request to completion.
    pub async fn run(&self, req: PipelineRequest) -> PipelineResult {
        use crate::pipeline::context::PipelineContext;
        use crate::pipeline::stage::{PipelineChain, PipelineStageEnum};
        use crate::pipeline::stages::{
            executor::UpstreamExecutorStage, quota::QuotaEnforcerStage, router::RouterStage,
            telemetry::TelemetryRecorderStage,
        };

        let ctx = PipelineContext::new(req, self.clone());
        let chain = PipelineChain::new(vec![
            PipelineStageEnum::TelemetryRecorder(TelemetryRecorderStage),
            PipelineStageEnum::Router(RouterStage),
            PipelineStageEnum::QuotaEnforcer(QuotaEnforcerStage),
            PipelineStageEnum::UpstreamExecutor(UpstreamExecutorStage),
        ]);

        match chain.execute(ctx).await {
            Ok(result) => result,
            Err(e) => {
                // Fallback if the chain entirely fails without catching
                self.failure(e, 1, ErrorPhase::Route)
            }
        }
    }

    pub(crate) async fn flatten_targets(
        &self,
        root_combo_id: &ComboId,
        targets: Vec<ComboTarget>,
    ) -> Result<Vec<ComboTarget>> {
        if !targets.iter().any(|t| t.sub_combo_id.is_some()) {
            return Ok(targets);
        }
        let conn_clone = self.conn.clone();
        let root_combo_id = *root_combo_id;
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::with_capacity(targets.len());
            let conn = conn_clone.lock();
            let mut visited: Vec<ComboId> = vec![root_combo_id];
            for t in targets {
                if let Some(sub_id) = t.sub_combo_id {
                    let sub_flat =
                        combos::resolve_combo_to_targets(&conn, sub_id, &mut visited, 0)?;
                    out.extend(sub_flat);
                } else {
                    out.push(t);
                }
            }
            let expanded = combos::expand_account_rotation(&conn, out)?;
            Ok(expanded)
        })
        .await
        .unwrap()
    }

    pub(crate) async fn auto_populate_if_empty(&self, combo: &Combo) -> Result<usize> {
        let conn_clone = self.conn.clone();
        let combo_id = combo.id;
        let combo_name = combo.name.clone();
        tokio::task::spawn_blocking(move || {
            {
                let conn = conn_clone.lock();
                if !combos::list_targets(&conn, combo_id)?.is_empty() {
                    return Ok(0);
                }
            }

            let added = {
                let conn = conn_clone.lock();
                combos::auto_populate_empty_combo(&conn, combo_id)?
            };

            if added > 0 {
                tracing::info!(
                    combo_id = combo_id.0,
                    combo_name = %combo_name,
                    added_targets = added,
                    "auto-populated empty combo with healthy provider's active models"
                );
            }
            Ok(added)
        })
        .await
        .unwrap()
    }

    pub async fn resolve_combo_targets_full(
        &self,
        eligible: Vec<ComboTarget>,
    ) -> Vec<crate::pipeline::context::ResolvedTarget> {
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

        let models_map = self
            .repo()
            .get_models_by_row_ids(&model_row_ids)
            .unwrap_or_default();
        let (accounts_map, kiro_map, antigravity_map) = self
            .repo()
            .get_accounts_meta(&account_ids)
            .unwrap_or_default();
        let providers_map = self
            .repo()
            .get_providers_auth_type(&provider_ids_no_account)
            .unwrap_or_default();

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
        })
        .await
        .unwrap()
    }

    pub(crate) async fn execute_single(
        &self,
        req: PipelineRequest,
        combo: &Combo,
        resolved_target: &crate::pipeline::context::ResolvedTarget,
        attempt: u8,
        race_size: u8,
        race_cancel: &CancellationToken,
    ) -> PipelineResult {
        let mut ctx = crate::pipeline::context::PipelineContext::new(req, self.clone());
        ctx.combo = Some(combo.clone());
        ctx.current_target = Some(resolved_target.clone());
        ctx.current_target_attempt = attempt;
        ctx.race_size = race_size;
        ctx.race_cancel = Some(race_cancel.clone());
        ctx.started = Some(std::time::Instant::now());

        if attempt > 1 {
            ctx.trace_id = format!("{}:retry{}", ctx.req.trace_id, attempt - 1);
        } else {
            ctx.trace_id = ctx.req.trace_id.to_string();
        }

        crate::usage::publish_stage_event(crate::usage::StageEvent {
            request_id: ctx.req.request_id.to_string(),
            trace_id: ctx.trace_id.to_string(),
            provider_id: resolved_target.target.provider_id.to_string(),
            upstream_model_id: resolved_target.model.model_id.as_str().to_string(),
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
        use crate::pipeline::stage::PipelineStageEnum;
        let chain = crate::pipeline::stage::PipelineChain::new(vec![
            PipelineStageEnum::OAuthRefresh(crate::pipeline::stages::target::OAuthRefreshStage),
            PipelineStageEnum::CustomAdapter(crate::pipeline::stages::target::CustomAdapterStage),
            PipelineStageEnum::TimeoutResolution(
                crate::pipeline::stages::target::TimeoutResolutionStage,
            ),
            PipelineStageEnum::Formatting(crate::pipeline::stages::target::FormattingStage),
            PipelineStageEnum::Dispatch(crate::pipeline::stages::target::DispatchStage),
        ]);

        match chain.execute_nested(&mut ctx).await {
            Ok(result) => result,
            Err(e) => {
                // Generic catch-all if a stage returns Err instead of Ok(PipelineResult with error)
                self.record_and_fail_with_trace_id(
                    ctx.req,
                    combo,
                    &resolved_target.target,
                    FailureContext {
                        proxy_url: None,
                        proxy_status: None,
                        attempt,
                        race_size,
                        err: &e,
                        started: ctx.started.unwrap(),
                        model: Some(&resolved_target.model),
                        connect_ms: None,
                        ttft_ms: None,
                        status_code: e.http_status(),
                    },
                    ctx.trace_id,
                )
            }
        }
    }

    pub(crate) async fn load_combo(&self, req: &PipelineRequest) -> Result<Combo> {
        if let Some(combo) = req.combo_override.as_ref() {
            return Ok(combo.clone());
        }
        let conn_clone = self.conn.clone();
        let combo_id = req.combo_id;
        tokio::task::spawn_blocking(move || {
            let conn = conn_clone.lock();
            combos::get_combo(&conn, combo_id)?.ok_or(CoreError::ComboNotFound(combo_id.0))
        })
        .await
        .unwrap()
    }

    pub(crate) async fn resolve_targets(
        &self,
        combo: &Combo,
        targets_override: Option<&[ComboTarget]>,
    ) -> Result<Vec<ComboTarget>> {
        let conn_clone = self.conn.clone();
        let combo_clone = combo.clone();
        let overrides = targets_override.map(|o| o.to_vec());
        let rr_counters = self.rr_counters.clone();
        let selection_registry = self.selection_registry.clone();
        tokio::task::spawn_blocking(move || {
            if let Some(overrides) = overrides {
                let conn = conn_clone.lock();
                return combos::expand_account_rotation(&conn, overrides);
            }

            let conn = conn_clone.lock();
            let _ = combos::list_targets(&conn, combo_clone.id)?;
            let ordered = combos::resolve_target_order_with_mode(
                &conn,
                &combo_clone,
                &rr_counters,
                &selection_registry,
            )?;
            combos::expand_account_rotation(&conn, ordered)
        })
        .await
        .unwrap()
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
                    Some(p) if p.id.0 == "opencode-zen" => Ok(String::new()),
                    _ => Err(CoreError::Auth(format!(
                        "combo_target {} has no account_id after expansion",
                        target.id.0
                    ))),
                }
            }
        }
    }

    pub(crate) fn resolve_target_api_key_and_label(
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
                    Some(p) if p.id.0 == "opencode-zen" => Ok((String::new(), None)),
                    _ => Err(CoreError::Auth(format!(
                        "combo_target {} has no account_id after expansion",
                        target.id.0
                    ))),
                }
            }
        }
    }

    pub(crate) fn failure(
        &self,
        err: CoreError,
        attempts: u8,
        _phase: ErrorPhase,
    ) -> PipelineResult {
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
        self.record_and_fail_with_trace_id_and_partial(
            req, combo, target, ctx, trace_id, None, None, 0, "",
        )
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
        self.tracker.record_and_fail_with_trace_id_and_partial(
            req, combo, target, ctx, trace_id, acc, chunk_id, created, model_name,
        )
    }
}
