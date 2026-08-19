use std::fmt::Write as _;
use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::HeaderMap,
    middleware::Next,
    response::Response,
};
use openproxy_core::routing::{self, RoutingPlan, SYNTHETIC_COMBO_ID};
use openproxy_types::combos::{Combo, ComboTarget};
use openproxy_types::{
    CoreError, OpenAIRequest,
    ids::{ApiKeyId, ComboId, RequestId},
};

use crate::{
    error::ApiError,
    middleware::auth::{ParsedChatRequest, ValidatedApiToken},
    state::AppState,
};

#[derive(Clone)]
pub struct ResolvedRoute {
    pub openai_req: Arc<OpenAIRequest>,
    pub combo_id: ComboId,
    pub combo_override: Option<Combo>,
    pub targets_override: Option<Vec<ComboTarget>>,
}

pub async fn routing_middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let parsed_chat_req = req
        .extensions()
        .get::<ParsedChatRequest>()
        .cloned()
        .ok_or_else(|| {
            ApiError(CoreError::Internal(
                "missing ParsedChatRequest in extensions".into(),
            ))
        })?;

    let auth_token = req.extensions().get::<ValidatedApiToken>().cloned();
    let api_key_id = auth_token.as_ref().map(|t| t.key_id);

    let openai_req = parsed_chat_req.parsed;
    let (plan, has_key_restrictions) =
        resolve_routing_plan(&state, req.headers(), &openai_req, auth_token.as_ref())?;
    let (combo_id, combo_override, targets_override) =
        translate_plan_to_targets(&state, plan, has_key_restrictions, api_key_id)?;

    let resolved = ResolvedRoute {
        openai_req,
        combo_id,
        combo_override,
        targets_override,
    };

    req.extensions_mut().insert(resolved);

    Ok(next.run(req).await)
}

fn resolve_routing_plan(
    state: &AppState,
    headers: &HeaderMap,
    openai_req: &OpenAIRequest,
    auth_result: Option<&ValidatedApiToken>,
) -> Result<(RoutingPlan, bool), ApiError> {
    let legacy_combo_name = headers
        .get("x-openproxy-combo")
        .and_then(|v| v.to_str().ok());

    let mut plan = {
        let w = state.db_pool().writer();
        if let Some(name) = legacy_combo_name {
            match openproxy_db::combos::get_combo_by_name(&w, name)? {
                Some(combo) => {
                    let targets = openproxy_db::combos::list_targets(&w, combo.id)?;
                    RoutingPlan::Combo {
                        combo_id: combo.id,
                        combo_name: combo.name,
                        strategy: combo.strategy,
                        race_size: combo.race_size,
                        targets,
                    }
                }
                None => return Err(ApiError(CoreError::ComboNotFound(0))),
            }
        } else {
            routing::resolve(&w, &openai_req.model)?
        }
    };

    let mut has_restrictions = false;
    if let RoutingPlan::Combo {
        combo_id, targets, ..
    } = &mut plan
        && let Some(auth) = auth_result
    {
        if !auth.is_combo_allowed(combo_id.0) {
            return Err(ApiError(CoreError::Auth(
                "combo not allowed for this key".to_string(),
            )));
        }

        let has_blacklist =
            auth.blacklisted_providers.is_some() || auth.blacklisted_models.is_some();
        let has_allowlist = auth.allowed_models.as_ref().is_some_and(|a| !a.is_empty());

        if has_blacklist || has_allowlist {
            has_restrictions = true;
            let r = state.db_pool().reader();
            let mut filtered_targets = Vec::with_capacity(targets.len());
            for target in targets.drain(..) {
                if !auth.is_provider_allowed(target.provider_id.as_str()) {
                    continue;
                }
                if let Some(row_id) = target.model_row_id {
                    let model = openproxy_core::models::get_by_row_id(&r, row_id)
                        .ok()
                        .flatten();
                    if let Some(m) = model
                        && !auth.is_model_allowed(
                            m.model_id.as_str(),
                            Some(target.provider_id.as_str()),
                        )
                    {
                        continue;
                    }
                }
                filtered_targets.push(target);
            }
            if filtered_targets.is_empty() {
                return Err(ApiError(CoreError::Auth(
                    "all upstream targets in combo are restricted for this key".to_string(),
                )));
            }
            *targets = filtered_targets;
        }
    }

    Ok((plan, has_restrictions))
}

pub type RoutingPlanTargets = (
    ComboId,
    Option<openproxy_types::Combo>,
    Option<Vec<openproxy_types::ComboTarget>>,
);

fn translate_plan_to_targets(
    state: &AppState,
    plan: RoutingPlan,
    has_key_restrictions: bool,
    api_key_id: Option<ApiKeyId>,
) -> Result<RoutingPlanTargets, ApiError> {
    match plan {
        RoutingPlan::Combo {
            combo_id,
            combo_name,
            strategy,
            race_size,
            targets,
        } => {
            if combo_id.0 == SYNTHETIC_COMBO_ID {
                let synthetic_combo = openproxy_types::combos::Combo {
                    id: combo_id,
                    name: combo_name,
                    strategy,
                    race_size,
                    created_at: String::new(),
                    context_window: None,
                    priority_mode: openproxy_types::combos::PriorityMode::Strict,
                    cooldown_mode: openproxy_types::config::CooldownMode::Flat,
                    cooldown_base_secs: None,
                    cooldown_max_secs: None,
                    cooldown_factor: None,
                    lkgp_exploration_rate: None,
                    selection_window_secs: None,
                };
                Ok((combo_id, Some(synthetic_combo), Some(targets)))
            } else if has_key_restrictions {
                Ok((combo_id, None, Some(targets)))
            } else {
                Ok((combo_id, None, None))
            }
        }
        RoutingPlan::NotFound { model, hint } => {
            record_model_not_found_usage_row(state, RequestId::new(), api_key_id, &model);
            let mut msg = format!("model not found: {model}");
            if let Some(h) = hint {
                let _ = write!(msg, " (hint: {h})");
            }
            Err(ApiError(CoreError::ModelNotFound {
                provider: "<unknown>".into(),
                model: msg,
            }))
        }
    }
}

fn record_model_not_found_usage_row(
    state: &AppState,
    request_id: RequestId,
    api_key_id: Option<ApiKeyId>,
    upstream_model: &str,
) {
    use openproxy_types::UsageInput;
    use openproxy_types::ids::{ProviderId, TraceId};
    let input = UsageInput {
        proxy_url: None,
        proxy_status: None,
        is_proxy_rotated: false,
        request_id,
        trace_id: TraceId::new().to_string(),
        attempt: 1,
        provider_id: ProviderId::new(""),
        account_id: None,
        combo_id: None,
        combo_target_id: None,
        model_row_id: None,
        upstream_model_id: upstream_model.to_string(),
        prompt_tokens: None,
        completion_tokens: None,
        cached_tokens: None,
        connect_ms: None,
        ttft_ms: None,
        total_ms: 0,
        status_code: 404,
        error_msg: Some("model_not_found".to_string()),
        race_total: 1,
        race_lost: false,
        api_key_id,
        request_body_json: None,
        response_body_json: None,
        request_headers: None,
        response_headers: None,
        error_message: Some("model_not_found".to_string()),
        race_attempts: 1,
        is_streaming: false,
        stream_complete: false,
        stop_reason: None,
        compression_savings_pct: None,
        compression_techniques: None,
        client_response: true,
        prompt_tokens_estimated: false,
        completion_tokens_estimated: false,
        endpoint_kind: openproxy_types::EndpointKind::Chat,
    };
    let Some(w) = state
        .db_pool()
        .try_writer_for(std::time::Duration::from_millis(100))
    else {
        tracing::warn!("hot-path writer lock timeout on model_not_found usage row; dropping");
        return;
    };
    let _ = openproxy_db::cost::record(&w, &input);
}
