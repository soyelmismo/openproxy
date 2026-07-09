use axum::{
    extract::{Request, State},
    http::HeaderMap,
    middleware::Next,
    response::Response,
};
use openproxy_core::{
    CoreError,
    combos::{Combo, ComboTarget},
    ids::{ApiKeyId, ComboId, RequestId},
    routing::{self, RoutingPlan, SYNTHETIC_COMBO_ID, build_synthetic_combo},
    translation::OpenAIRequest,
};
use std::sync::Arc;

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

    let openai_req: OpenAIRequest = serde_json::from_slice(&parsed_chat_req.bytes)
        .map_err(|e| ApiError(CoreError::Parse(e.to_string())))?;

    let plan = resolve_routing_plan(&state, req.headers(), &openai_req, &auth_token)?;
    let (combo_id, combo_override, targets_override) =
        translate_plan_to_targets(&state, &plan, api_key_id)?;

    let resolved = ResolvedRoute {
        openai_req: Arc::new(openai_req),
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
    auth_result: &Option<ValidatedApiToken>,
) -> Result<RoutingPlan, ApiError> {
    let legacy_combo_name = headers
        .get("x-openproxy-combo")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let plan = {
        let w = state.db_pool().writer();
        if let Some(name) = legacy_combo_name.as_deref() {
            match openproxy_core::combos::get_combo_by_name(&w, name)? {
                Some(combo) => {
                    let targets = openproxy_core::combos::list_targets(&w, combo.id)?;
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

    if let RoutingPlan::Combo { combo_id, .. } = &plan
        && let Some(auth) = auth_result
        && let Some(allowed) = &auth.allowed_combos
        && !allowed.is_empty()
        && !allowed.contains(&combo_id.0)
    {
        return Err(ApiError(CoreError::Auth(
            "combo not allowed for this key".to_string(),
        )));
    }

    Ok(plan)
}

#[allow(clippy::type_complexity)]
fn translate_plan_to_targets(
    state: &AppState,
    plan: &RoutingPlan,
    api_key_id: Option<ApiKeyId>,
) -> Result<
    (
        ComboId,
        Option<openproxy_core::combos::Combo>,
        Option<Vec<openproxy_core::combos::ComboTarget>>,
    ),
    ApiError,
> {
    match plan {
        RoutingPlan::Direct {
            provider_id,
            account_id,
            model_row_id,
            ..
        } => {
            let (synthetic_combo, synthetic_targets) =
                build_synthetic_combo(provider_id.clone(), *account_id, *model_row_id);
            Ok((
                ComboId(SYNTHETIC_COMBO_ID),
                Some(synthetic_combo),
                Some(synthetic_targets),
            ))
        }
        RoutingPlan::Combo { combo_id, .. } => Ok((*combo_id, None, None)),
        RoutingPlan::NotFound { model, hint } => {
            let _ = record_model_not_found_usage_row(state, RequestId::new(), api_key_id, model);
            let mut msg = format!("model not found: {}", model);
            if let Some(h) = hint {
                msg.push_str(&format!(" (hint: {})", h));
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
) -> std::result::Result<(), ApiError> {
    use openproxy_core::{
        cost::{self, UsageInput},
        ids::{ProviderId, TraceId},
    };
    let input = UsageInput {
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
        endpoint_kind: openproxy_core::endpoint::EndpointKind::Chat,
    };
    let w = match state
        .db_pool()
        .try_writer_for(std::time::Duration::from_millis(100))
    {
        Some(w) => w,
        None => {
            tracing::warn!("hot-path writer lock timeout on model_not_found usage row; dropping");
            return Ok(());
        }
    };
    let _ = cost::record(&w, &input).map_err(ApiError);
    Ok(())
}
