use super::{Deserialize, AppState, ApiError, CoreError, core_models, ModelRowId, AccountId, Arc, core_providers, core_accounts, core_oauth, adapters, RequestId, TraceId, ComboId, refresh_oauth_if_needed, ProviderId};
use axum::{
    Json,
    extract::{Path, Query, State},
};

use openproxy_core::admin as core_admin;

pub async fn toggle_model(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let active = body
        .get("active")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| CoreError::Validation("missing 'active' bool".into()))?;
    let w = s.db_pool().writer();
    core_models::set_active(&w, ModelRowId(id), active)?;
    Ok(Json(serde_json::json!({ "id": id, "active": active })))
}

pub async fn bulk_toggle_models(
    State(s): State<AppState>,
    Json(body): Json<core_admin::BulkToggleInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let w = s.db_pool().writer();
    let updated = core_admin::set_active_bulk(&w, body)?;
    Ok(Json(serde_json::json!({
        "updated": updated,
    })))
}

pub async fn delete_model(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let w = s.db_pool().writer();
    let removed = core_models::delete(&w, ModelRowId(id))?;
    Ok(Json(serde_json::json!({ "id": id, "deleted": removed })))
}

pub async fn update_model(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(input): Json<core_admin::UpdateModelInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let w = s.db_pool().writer();
    core_admin::update_model(&w, ModelRowId(id), input)?;
    Ok(Json(serde_json::json!({ "id": id, "updated": true })))
}

pub async fn create_custom_model(
    State(s): State<AppState>,
    Json(input): Json<core_admin::CreateCustomModelInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let w = s.db_pool().writer();
    let row_id = core_admin::create_custom_model(&w, input)?;
    Ok(Json(serde_json::json!({ "row_id": row_id.0 })))
}

pub async fn test_model(
    State(s): State<AppState>,
    Path(model_row_id): Path<i64>,
    cancel_watch: Option<axum::Extension<crate::disconnect::CancelWatch>>,
    body_bytes: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    let cancel_rx = cancel_watch.map(|axum::Extension(cw)| cw.rx);

    let (account_id, proxy_url) = if body_bytes.is_empty() {
        (None, None)
    } else {
        match serde_json::from_slice::<TestModelInput>(&body_bytes) {
            Ok(input) => {
                let aid = input.account_id.map(AccountId::new);
                let purl = if let Some(ref pid) = input.proxy_id {
                    tokio::task::block_in_place(|| {
                        let r = s.db_pool().reader();
                        if let Ok(Some(p)) = openproxy_core::free_proxies::get_proxy(&r, pid) {
                            Some(format!(
                                "{}://{}:{}",
                                p.r#type.to_lowercase(),
                                p.host,
                                p.port
                            ))
                        } else {
                            None
                        }
                    })
                } else {
                    None
                };
                (aid, purl)
            }
            Err(e) => {
                return Err(ApiError(CoreError::Parse(format!("Invalid JSON: {e}"))));
            }
        }
    };

    let (r, debug_payload) = run_test_for_model(
        &s,
        model_row_id,
        account_id,
        proxy_url,
        TestOptions::default(),
        cancel_rx,
    )
    .await;
    Ok(Json(serde_json::json!({
        "row_id": r.row_id,
        "status": r.status,
        "elapsed_ms": r.elapsed_ms,
        "error_msg": r.error_msg,
        "debug_payload": debug_payload,
    })))
}

pub async fn list_models_admin(
    State(s): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<ListModelsQuery>,
) -> Result<Json<Vec<core_models::Model>>, ApiError> {
    // Read-only SELECT — use the READER.
    let r = s.db_pool().reader();
    let mut list = core_models::list_all(&r)?;
    if let Some(p) = q.provider_id {
        list.retain(|m| m.provider_id.as_str() == p);
    }
    Ok(Json(list))
}

pub async fn sync_models_dev(
    State(s): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let upstream = Arc::clone(s.upstream_client());
    let db_pool = Arc::clone(s.db_pool());
    let result = openproxy_core::models_dev_sync::run_one_shot(db_pool, upstream).await;
    let msg = match result {
        Ok(m) => m,
        Err(e) => return Err(ApiError(e)),
    };
    Ok(Json(serde_json::json!({ "message": msg })))
}

/// Query string for `POST /admin/models/:id/refresh` — lets the caller
/// override the refresh TTL in seconds and pin a specific account.
#[derive(Debug, Default, Deserialize)]
pub struct RefreshQuery {
    /// Cache TTL in seconds for the discovered rows. Defaults to 1 hour.
    pub ttl_seconds: Option<i64>,
    /// Account id whose API key will be used. Required when the provider
    /// has more than one account; otherwise the first account wins. The
    /// API key is decrypted on the fly and is never logged or echoed.
    pub account_id: Option<i64>,
}

/// `GET /admin/models` — every row in the `models` table.
#[derive(Debug, Default, Deserialize)]
pub struct ListModelsQuery {
    pub provider_id: Option<String>,
}

/// `POST /admin/models/:id/test` input
#[derive(Debug, Default, Deserialize)]
pub struct TestModelInput {
    pub account_id: Option<i64>,
    pub proxy_id: Option<String>,
}

/// Maximum number of characters from a failing response body that we
/// surface back to the dashboard.
pub const TEST_ERROR_BODY_MAX_CHARS: usize = 512;

/// The outcome of a single test ping.
#[derive(Debug, Clone)]
pub struct TestResult {
    pub row_id: i64,
    pub status: u16,
    pub elapsed_ms: u64,
    pub error_msg: Option<String>,
    pub skipped: bool,
    pub skip_reason: Option<String>,
}

impl TestResult {
    pub fn skipped(row_id: i64, reason: &str) -> Self {
        Self {
            row_id,
            status: 0,
            elapsed_ms: 0,
            error_msg: Some(reason.to_string()),
            skipped: true,
            skip_reason: Some(reason.to_string()),
        }
    }
}

/// Knobs that distinguish the per-row test path from the
/// per-combo fan-out.
#[derive(Debug, Clone, Copy, Default)]
pub struct TestOptions {
    pub in_combo_fanout: bool,
}

pub async fn refresh_models(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<RefreshQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    run_refresh(s, id, q).await
}

pub(crate) async fn run_test_for_model(
    s: &AppState,
    model_row_id: i64,
    account_id: Option<AccountId>,
    proxy_url: Option<String>,
    opts: TestOptions,
    cancel_rx: Option<tokio::sync::watch::Receiver<Option<openproxy_types::CancelReason>>>,
) -> (TestResult, Option<serde_json::Value>) {
    use openproxy_adapters::adapters::gemini::openai_to_gemini;
    use openproxy_pipeline::translation::openai_to_anthropic;
    use openproxy_types::{OpenAIMessage, OpenAIRequest};

    let row_id = ModelRowId(model_row_id);
    let start = std::time::Instant::now();

    // 1. Load the model row.
    let res = {
        let r = s.db_pool().reader();
        match core_models::get_by_row_id(&r, row_id) {
            Ok(Some(m)) => Ok(m),
            Ok(None) => Err(ApiError(CoreError::ModelNotFound {
                provider: "<unknown>".into(),
                model: format!("row_id={model_row_id}"),
            })),
            Err(e) => Err(ApiError(e)),
        }
    };
    let model = match res {
        Ok(m) => m,
        Err(ApiError(e)) => {
            return (
                TestResult {
                    row_id: model_row_id,
                    status: e.http_status(),
                    elapsed_ms: 0,
                    error_msg: Some(openproxy_core::cost::redact_error_msg(&e.to_string()).0),
                    skipped: true,
                    skip_reason: Some(format!(
                        "model lookup failed: {}",
                        openproxy_core::cost::redact_error_msg(&e.to_string()).0
                    )),
                },
                None,
            );
        }
    };

    // 1a. If the model is toggled inactive, the per-row handler
    //     would still let the operator fire a test (they may be
    //     debugging why a model went inactive). The combo handler,
    //     however, wants to skip these rows outright — a fan-out
    //     should not bombard a model the operator has explicitly
    //     deactivated. We can detect which caller we are by
    //     inspecting `account_id`: a `Some(_)` value came from the
    //     combo path (the target row had a pinned account), while
    //     `None` means the per-row handler is asking us to pick.
    //     A pinned account means "this is a real target, respect
    //     its active flag"; no pinned account means "the operator
    //     clicked the button, do what they ask". This is a
    //     lightweight heuristic that keeps both flows happy without
    //     adding a new parameter to the helper signature.
    if !model.active && opts.in_combo_fanout {
        return (TestResult::skipped(model_row_id, "model is inactive"), None);
    }

    // 2. Find the adapter for that provider. Check built-in adapters
    //    first, then fall back to constructing a CustomAdapter from the
    //    DB row.
    let adapter = match resolve_adapter(s, &model.provider_id, s.adapters().as_slice()) {
        Ok(a) => a,
        Err(err) => {
            return (
                TestResult {
                    row_id: model_row_id,
                    status: err.http_status(),
                    elapsed_ms: 0,
                    error_msg: Some(openproxy_core::cost::redact_error_msg(&err.to_string()).0),
                    skipped: true,
                    skip_reason: Some(openproxy_core::cost::redact_error_msg(&err.to_string()).0),
                },
                None,
            );
        }
    };

    // 3. Resolve the account to use. Anonymous access is allowed when:
    //      - provider has auth_type "none", OR
    //      - provider has no accounts configured (fallback to anonymous)
    //    This lets bearer providers like opencode-zen work without
    //    accounts while still using accounts when they exist.
    let (is_anonymous, accounts_list) = {
        let r = s.db_pool().reader();
        let provider_row = core_providers::get(&r, &model.provider_id).unwrap_or_default();
        let accs = core_accounts::list(&r, Some(&model.provider_id), s.master_key().as_ref())
            .unwrap_or_default();
        let anon = match &provider_row {
            Some(p) if matches!(p.auth_type, core_providers::AuthType::None) => true,
            _ if accs.is_empty() => true, // No accounts → try anonymous
            _ => false,
        };
        (anon, accs)
    };

    // Capture the optional account_id AND its label. The label is
    // needed by providers whose URL embeds account-level metadata
    // (e.g. CloudFlare Workers AI uses the label as its account ID).
    let mut raw_account_opt = None;
    let (_account_id_opt, account_label, api_key) = if is_anonymous {
        (None, String::new(), String::new()) // Anonymous: no account, empty key
    } else {
        let account_id = match account_id {
            Some(id) => Some(id),
            None => {
                let healthy = accounts_list
                    .iter()
                    .find(|a| a.health_status == core_accounts::HealthStatus::Healthy);
                let degraded = || {
                    accounts_list
                        .iter()
                        .find(|a| a.health_status == core_accounts::HealthStatus::Degraded)
                };
                healthy
                    .or_else(degraded)
                    .or_else(|| accounts_list.first())
                    .map(|a| a.id)
            }
        };

        // 4. Decrypt the API key. Drop the reader guard immediately.
        //    OAuth accounts store the token in access_token_encrypted,
        //    not api_key_encrypted, so we fall back to that if the
        //    primary decrypt fails (e.g. NULL column).
        let api_key = match account_id {
            Some(aid) => {
                let account = tokio::task::block_in_place(|| {
                    let r = s.db_pool().reader();
                    core_accounts::get(&r, aid, s.master_key().as_ref())
                        .ok()
                        .flatten()
                });
                raw_account_opt = account;
                if let Some(ref acc) = raw_account_opt
                    && acc.auth_type == "oauth"
                {
                    match core_oauth::resolve_oauth_token(
                        s.db_pool().as_ref(),
                        acc,
                        model.provider_id.as_str(),
                        s.oauth_provider_registry().as_ref(),
                        s.upstream_client(),
                        s.master_key().as_ref(),
                    )
                    .await
                    {
                        Ok(token) => token,
                        Err(e) => {
                            let elapsed_ms = start.elapsed().as_millis() as u64;
                            let err_msg = format!("resolve oauth token: {e}");
                            return (
                                TestResult {
                                    row_id: model_row_id,
                                    status: e.http_status(),
                                    elapsed_ms,
                                    error_msg: Some(err_msg),
                                    skipped: false,
                                    skip_reason: None,
                                },
                                None,
                            );
                        }
                    }
                } else {
                    match {
                        let r = s.db_pool().reader();
                        core_accounts::decrypt_api_key(&r, aid, s.master_key().as_ref()).or_else(
                            |_| {
                                core_accounts::decrypt_access_token(
                                    &r,
                                    aid,
                                    s.master_key().as_ref(),
                                )
                            },
                        )
                    }
                    .map_err(ApiError)
                    {
                        Ok(k) => k,
                        Err(ApiError(e)) => {
                            return (
                                TestResult {
                                    row_id: model_row_id,
                                    status: e.http_status(),
                                    elapsed_ms: 0,
                                    error_msg: Some(
                                        openproxy_core::cost::redact_error_msg(&e.to_string()).0,
                                    ),
                                    skipped: true,
                                    skip_reason: Some(
                                        openproxy_core::cost::redact_error_msg(&e.to_string()).0,
                                    ),
                                },
                                None,
                            );
                        }
                    }
                }
            }
            None => String::new(),
        };

        let account_label = raw_account_opt
            .as_ref()
            .and_then(|a| a.label.clone())
            .unwrap_or_default();

        (account_id, account_label, api_key)
    };

    // 5. Build the minimal test request. The exact prompts and limits
    //    are not significant — we just need the upstream to issue a
    //    real HTTP call so we can record the result.
    //
    //    The `system` message is sent first because some OpenRouter-
    //    served models (e.g. certain NVIDIA Nemotron builds) reject a
    //    bare `[{role: "user", content: "ping"}]` with a 400 from the
    //    OpenAI Python SDK v1.x Pydantic validator: the validator's
    //    discriminated-union ordering tries `developer` first when a
    //    `name: null` field is present, then complains the role is
    //    not `"developer"`. Adding a system message changes the
    //    validator's selection to the `system` variant (or, for
    //    non-strict validators, bypasses the discriminator) so the
    //    `user` message is accepted as-is. This matches the wire
    //    shape production clients (OpenAI SDK, Anthropic SDK, etc.)
    //    send, and the system prompt is also what most providers
    //    expect as a sanity check.
    let openai_req = OpenAIRequest {
        model: model.model_id.as_str().to_string(),
        messages: vec![OpenAIMessage {
            role: "user".into(),
            content: Some(serde_json::Value::String("hi".to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::new(),
        }],
        stream: false,
        temperature: None,
        max_tokens: Some(16),
        top_p: None,
        stop: None,
        tools: None,
        tool_choice: None,
        top_k: None,
        user: None,
        extra: serde_json::Map::new(),
    };

    // 6. Test supported for all providers via standard or wrap_request_body path.

    // 7. Standard adapter path: translate to the row's native format
    //    and assemble the URL. This works for all non-custom providers
    //    (OpenAI-compatible, Anthropic, Gemini).
    //    `serde_json::to_value` cannot fail for these struct shapes in
    //    practice, but we still want a typed error if it ever does.
    let effective_target_format = match adapter.format() {
        adapters::AdapterFormat::Openai => openproxy_core::models::TargetFormat::Openai,
        adapters::AdapterFormat::Anthropic => openproxy_core::models::TargetFormat::Anthropic,
        adapters::AdapterFormat::Mixed => model.target_format,
        adapters::AdapterFormat::Gemini => openproxy_core::models::TargetFormat::Gemini,
        adapters::AdapterFormat::Responses => openproxy_core::models::TargetFormat::Responses,
        adapters::AdapterFormat::Atomesus => openproxy_core::models::TargetFormat::Atomesus,
    };
    let (url, body_value): (String, serde_json::Value) = if effective_target_format
        == openproxy_core::models::TargetFormat::Anthropic
    {
        let anthropic_req = openai_to_anthropic(
            &openai_req,
            model.model_id.as_str(),
            &openai_req.messages,
            openai_req.stream,
        );
        let url = adapter.build_chat_url_for_account(
            openproxy_core::models::TargetFormat::Anthropic,
            &model.model_id,
            &account_label,
        );
        match serde_json::to_value(&anthropic_req) {
            Ok(v) => (url, v),
            Err(e) => {
                let err = CoreError::Internal(format!("serialize anthropic req: {e}"));
                return (
                    TestResult {
                        row_id: model_row_id,
                        status: 500,
                        elapsed_ms: 0,
                        error_msg: Some(openproxy_core::cost::redact_error_msg(&err.to_string()).0),
                        skipped: true,
                        skip_reason: Some(
                            openproxy_core::cost::redact_error_msg(&err.to_string()).0,
                        ),
                    },
                    None,
                );
            }
        }
    } else if effective_target_format == openproxy_core::models::TargetFormat::Gemini {
        let gemini_req = openai_to_gemini(&openai_req, &openai_req.messages);
        let url = adapter.build_chat_url_for_account(
            openproxy_core::models::TargetFormat::Gemini,
            &model.model_id,
            &account_label,
        );
        match serde_json::to_value(&gemini_req) {
            Ok(v) => (url, v),
            Err(e) => {
                let err = CoreError::Internal(format!("serialize gemini req: {e}"));
                return (
                    TestResult {
                        row_id: model_row_id,
                        status: 500,
                        elapsed_ms: 0,
                        error_msg: Some(openproxy_core::cost::redact_error_msg(&err.to_string()).0),
                        skipped: true,
                        skip_reason: Some(
                            openproxy_core::cost::redact_error_msg(&err.to_string()).0,
                        ),
                    },
                    None,
                );
            }
        }
    } else if effective_target_format == openproxy_core::models::TargetFormat::Responses {
        let url = adapter.build_chat_url_for_account(
            openproxy_core::models::TargetFormat::Responses,
            &model.model_id,
            &account_label,
        );
        let mut responses_req = openai_req;
        responses_req.max_tokens = None;
        let (_cancel_tx, client_disconnected) =
            tokio::sync::watch::channel::<Option<openproxy_types::CancelReason>>(None);
        let pipeline_req = openproxy_pipeline::PipelineRequest {
            request_id: RequestId::new(),
            trace_id: TraceId::new(),
            combo_id: ComboId(0),
            openai_request: std::sync::Arc::new(responses_req),
            client_disconnected,
            stream_sink: None,
            api_key_id: None,
            race_cancel: None,
            combo_override: None,
            targets_override: None,
            request_headers: std::collections::BTreeMap::new(),
            request_body_json: None,
            race_cancelled: false,
            endpoint_kind: openproxy_types::EndpointKind::Chat,
            compressed_messages: std::sync::Arc::new(std::sync::OnceLock::new()),
            proxy_override: None,
        };
        let formatter = openproxy_pipeline::formatting::get_formatter(
            openproxy_core::models::TargetFormat::Responses,
        );
        match formatter.format_request(
            &pipeline_req,
            &model,
            &pipeline_req.openai_request.messages,
            true,
            &adapter,
        ) {
            Ok(req_bytes) => match serde_json::from_slice::<serde_json::Value>(&req_bytes) {
                Ok(v) => (url, v),
                Err(e) => {
                    let err = CoreError::Internal(format!("serialize responses req: {e}"));
                    return (
                        TestResult {
                            row_id: model_row_id,
                            status: 500,
                            elapsed_ms: 0,
                            error_msg: Some(
                                openproxy_core::cost::redact_error_msg(&err.to_string()).0,
                            ),
                            skipped: true,
                            skip_reason: Some(
                                openproxy_core::cost::redact_error_msg(&err.to_string()).0,
                            ),
                        },
                        None,
                    );
                }
            },
            Err(err) => {
                return (
                    TestResult {
                        row_id: model_row_id,
                        status: 500,
                        elapsed_ms: 0,
                        error_msg: Some(openproxy_core::cost::redact_error_msg(&err.to_string()).0),
                        skipped: true,
                        skip_reason: Some(
                            openproxy_core::cost::redact_error_msg(&err.to_string()).0,
                        ),
                    },
                    None,
                );
            }
        }
    } else {
        let url = adapter.build_chat_url_for_account(
            openproxy_core::models::TargetFormat::Openai,
            &model.model_id,
            &account_label,
        );
        match serde_json::to_value(&openai_req) {
            Ok(v) => (url, v),
            Err(e) => {
                let err = CoreError::Internal(format!("serialize openai req: {e}"));
                return (
                    TestResult {
                        row_id: model_row_id,
                        status: 500,
                        elapsed_ms: 0,
                        error_msg: Some(openproxy_core::cost::redact_error_msg(&err.to_string()).0),
                        skipped: true,
                        skip_reason: Some(
                            openproxy_core::cost::redact_error_msg(&err.to_string()).0,
                        ),
                    },
                    None,
                );
            }
        }
    };

    // 8. Build the HTTP request. The 15s timeout caps the test wall-
    //    clock cost — a hung upstream shouldn't pin a dashboard
    //    button indefinitely.
    // Headers will be built below after resolving custom_meta

    let mut custom_meta = None;
    if model.provider_id.as_str() == "antigravity" {
        let project = raw_account_opt
            .as_ref()
            .and_then(|a| a.oauth_provider_specific.as_ref())
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .and_then(|v| {
                v.get("project_id")
                    .or_else(|| v.get("project"))
                    .or_else(|| v.get("projectId"))
                    .or_else(|| v.get("client_id"))
                    .or_else(|| v.get("clientId"))
                    .and_then(|p| p.as_str().map(String::from))
            });

        custom_meta = Some(openproxy_types::context::CustomProviderMeta {
            access_token: api_key.clone(),
            maybe_refresh: None,
            kiro_region: None,
            kiro_profile_arn: None,
            antigravity_project: project,
            antigravity_metadata: None,
            codex_workspace_id: None,
        });
    } else if model.provider_id.as_str() == "kiro" {
        let (region, profile_arn) = raw_account_opt
            .as_ref()
            .and_then(|a| a.oauth_provider_specific.as_ref())
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .map_or((None, None), |v| {
                let r = v.get("region").and_then(|x| x.as_str()).map(String::from);
                let p = v
                    .get("profileArn")
                    .or_else(|| v.get("profile_arn"))
                    .and_then(|x| x.as_str())
                    .map(String::from);
                (r, p)
            });

        custom_meta = Some(openproxy_types::context::CustomProviderMeta {
            access_token: api_key.clone(),
            maybe_refresh: None,
            kiro_region: region,
            kiro_profile_arn: profile_arn,
            antigravity_project: None,
            antigravity_metadata: None,
            codex_workspace_id: None,
        });
    }

    let headers = adapter.build_headers(&api_key, effective_target_format, &model.model_id);

    let dummy_target = openproxy_types::context::ResolvedTarget {
        target: openproxy_types::combos::ComboTarget {
            id: openproxy_types::ids::ComboTargetId(0),
            combo_id: openproxy_types::ids::ComboId(0),
            provider_id: openproxy_types::ids::ProviderId::new(model.provider_id.as_str()),
            account_id: None,
            model_row_id: None,
            sub_combo_id: None,
            priority_order: 0,
            weight: 1,
            active: true,
            rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
        },
        model,
        api_key,
        api_key_label: Some(account_label),
        custom_meta,
    };

    let mut req = openproxy_adapters::upstream::UpstreamRequest::post_json(
        &url,
        match serde_json::to_vec(&body_value) {
            Ok(b) => {
                match adapter.wrap_request_body(
                    bytes::Bytes::from(b),
                    effective_target_format,
                    &dummy_target.model.model_id,
                    &dummy_target,
                ) {
                    Ok(wrapped) => wrapped,
                    Err(e) => {
                        return (
                            TestResult {
                                row_id: model_row_id,
                                status: 500,
                                elapsed_ms: 0,
                                error_msg: Some(
                                    openproxy_core::cost::redact_error_msg(&format!(
                                        "failed to wrap request: {e}"
                                    ))
                                    .0,
                                ),
                                skipped: true,
                                skip_reason: Some(
                                    openproxy_core::cost::redact_error_msg(&format!(
                                        "failed to wrap request: {e}"
                                    ))
                                    .0,
                                ),
                            },
                            None,
                        );
                    }
                }
            }
            Err(e) => {
                return (
                    TestResult {
                        row_id: model_row_id,
                        status: 500,
                        elapsed_ms: 0,
                        error_msg: Some(
                            openproxy_core::cost::redact_error_msg(&format!(
                                "failed to serialize request: {e}"
                            ))
                            .0,
                        ),
                        skipped: true,
                        skip_reason: Some(
                            openproxy_core::cost::redact_error_msg(&format!(
                                "failed to serialize request: {e}"
                            ))
                            .0,
                        ),
                    },
                    None,
                );
            }
        },
    );
    req.proxy = proxy_url;
    for (k, v) in &headers {
        if let Ok(hn) = axum::http::HeaderName::from_bytes(k.as_bytes())
            && let Ok(hv) = axum::http::HeaderValue::from_str(v)
        {
            req.headers.insert(hn, hv);
        }
    }

    // 9. Send + measure. We capture both the wall-clock elapsed time
    //    and a truncated error body so the dashboard can show
    //    something useful when the upstream is unhappy.
    let start = std::time::Instant::now();
    let client = s.upstream_client();
    let cancel = openproxy_adapters::upstream::CancellationToken::new();

    if let Some(mut rx) = cancel_rx {
        let rx_cancel = openproxy_adapters::upstream::CancellationToken::clone(&cancel);
        tokio::spawn(async move {
            if rx.borrow().is_some() {
                rx_cancel.cancel();
                return;
            }
            while rx.changed().await.is_ok() {
                if rx.borrow().is_some() {
                    rx_cancel.cancel();
                    return;
                }
            }
        });
    }

    let profile = openproxy_adapters::upstream::TimeoutProfile::Custom(
        openproxy_adapters::upstream::ResolvedTimeouts {
            dns_ms: 2000,
            dial_ms: 5000,
            tls_ms: 5000,
            write_ms: 5000,
            headers_ms: 15000,
            body_chunk_ms: 5000,
            total_ms: 15000,
        },
    );

    let request_headers_map = if opts.in_combo_fanout {
        None
    } else {
        Some(
            req.headers
                .iter()
                .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                .collect::<std::collections::HashMap<_, _>>(),
        )
    };

    let result = client.call(req, profile, cancel).await;
    let elapsed_ms = start.elapsed().as_millis() as u64;
    let mut debug_payload = None;
    if let Some(req_headers) = request_headers_map {
        debug_payload = Some(serde_json::json!({
            "request_headers": req_headers,
            "request_url": url,
            "request_body": serde_json::from_slice::<serde_json::Value>(&body_value.to_string().into_bytes()).unwrap_or_else(|_| serde_json::json!(body_value.to_string())),
        }));
    }

    let (status, error_msg) = match result {
        Ok(response) => {
            let status = response.status.as_u16();
            if status >= 400 {
                let body = response.collect().await.unwrap_or_default();
                let text = String::from_utf8_lossy(&body);
                if let Some(dp) = debug_payload.as_mut() {
                    dp["response_body"] = serde_json::from_str(&text)
                        .unwrap_or_else(|_| serde_json::json!(text.to_string()));
                }
                let truncated: String = text.chars().take(TEST_ERROR_BODY_MAX_CHARS).collect();
                (status, Some(truncated))
            } else {
                (status, None)
            }
        }
        Err(e) => {
            // 0 = "request never reached the upstream" (DNS / connect / TLS
            // / timeout). The schema doesn't constrain this — `0` is a
            // distinct sentinel that the dashboard renders as a network
            // error.
            (0, Some(format!("{e:?}")))
        }
    };

    // 10. Persist the result. The persist is independent of the response
    //     shape: the dashboard should always see *something* on the row
    //     after the button is pressed. We write to the row from the
    //     per-row path only; the combo fan-out does not want its
    //     transient probe to overwrite the row's last-test status.
    if !opts.in_combo_fanout {
        let status_i32 = i32::from(status);
        if let Err(e) = {
            let w = s.db_pool().writer();
            core_models::set_test_status(&w, row_id, status_i32)
        } {
            return (
                TestResult {
                    row_id: model_row_id,
                    status: e.http_status(),
                    elapsed_ms,
                    error_msg: Some(openproxy_core::cost::redact_error_msg(&e.to_string()).0),
                    skipped: true,
                    skip_reason: Some(openproxy_core::cost::redact_error_msg(&e.to_string()).0),
                },
                None,
            );
        }
    }

    (
        TestResult {
            row_id: model_row_id,
            status,
            elapsed_ms,
            error_msg,
            skipped: false,
            skip_reason: None,
        },
        debug_payload,
    )
}

pub(crate) async fn run_refresh(
    s: AppState,
    id: i64,
    q: RefreshQuery,
) -> Result<Json<serde_json::Value>, ApiError> {
    let row_id = ModelRowId(id);
    let ttl_seconds = q.ttl_seconds.unwrap_or(3_600);

    // 1. Look up the model to find the provider.
    let provider_id = {
        let w = s.db_pool().writer();
        let found = match core_models::get_by_row_id(&w, row_id) {
            Ok(opt) => opt,
            Err(e) => return Err(ApiError(e)),
        };
        match found {
            Some(m) => m.provider_id,
            None => {
                return Err(ApiError(CoreError::ModelNotFound {
                    provider: "<unknown>".into(),
                    model: format!("row_id={}", row_id.0),
                }));
            }
        }
    };

    // 2. Find the adapter for that provider. Check built-in adapters
    //    first, then fall back to constructing a CustomAdapter from the
    //    DB row.
    let adapter = match resolve_adapter(&s, &provider_id, s.adapters().as_slice()) {
        Ok(a) => a,
        Err(e) => return Err(ApiError(e)),
    };

    // 3. Resolve an account and decrypt/refresh its credential.
    let selected_account_id = {
        let w = s.db_pool().writer();

        let provider_row = match core_providers::get(&w, &provider_id) {
            Ok(p) => p,
            Err(e) => return Err(ApiError(e)),
        };
        let accounts_list =
            match core_accounts::list(&w, Some(&provider_id), s.master_key().as_ref()) {
                Ok(l) => l,
                Err(e) => return Err(ApiError(e)),
            };

        let is_anonymous = match &provider_row {
            Some(p) if matches!(p.auth_type, core_providers::AuthType::None) => true,
            _ if accounts_list.is_empty() => true,
            _ => false,
        };

        if is_anonymous {
            None
        } else {
            match q.account_id {
                Some(aid) => Some(AccountId::new(aid)),
                None => accounts_list.first().map(|a| a.id),
            }
        }
    };

    let api_key = match selected_account_id {
        Some(account_id) => {
            let account = {
                let w = s.db_pool().writer();
                match core_accounts::get(&w, account_id, s.master_key().as_ref()) {
                    Ok(Some(a)) => a,
                    Ok(None) => {
                        return Err(ApiError(CoreError::AccountNotFound(account_id.0)));
                    }
                    Err(e) => return Err(ApiError(e)),
                }
            };
            if account.auth_type == "oauth" {
                refresh_oauth_if_needed(&s, account, &provider_id).await
            } else {
                let w = s.db_pool().writer();
                match core_accounts::decrypt_api_key(&w, account_id, s.master_key().as_ref()) {
                    Ok(k) => k,
                    Err(e) => return Err(ApiError(e)),
                }
            }
        }
        None => String::new(),
    };

    // Resolve account label for CloudFlare / label-based providers.
    let account_label = match selected_account_id {
        Some(account_id) => {
            let w = s.db_pool().writer();
            match core_accounts::get(&w, account_id, s.master_key().as_ref()) {
                Ok(Some(a)) => a.label.unwrap_or_default(),
                _ => String::new(),
            }
        }
        None => String::new(),
    };
    // 4. Run the refresh. `core_admin::refresh_models` takes the connection
    //    by value (not by reference) so the future is `Send`-able
    //    end to end: `rusqlite::Connection: !Sync` (it has internal
    //    `RefCell`s), and a `&Connection` borrowed across the await
    //    would propagate `!Send` to the outer future, breaking axum's
    //    `Handler` trait. We open a fresh handle via `DbPool::open_connection`
    //    and pass it by value; the writer mutex is unaffected.
    let conn_for_refresh = match s.db_pool().open_connection() {
        Ok(c) => c,
        Err(e) => return Err(ApiError(e)),
    };
    let upsert = match core_admin::refresh_models(
        conn_for_refresh,
        &provider_id,
        &api_key,
        &adapter,
        s.upstream_client(),
        ttl_seconds,
        &account_label,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return Err(ApiError(e)),
    };

    Ok(Json(serde_json::json!({
        "touched": upsert.touched,
        "new_model_ids": upsert.new_model_ids,
        "provider_id": provider_id.as_str(),
    })))
}

pub(crate) fn resolve_adapter(
    s: &AppState,
    provider_id: &ProviderId,
    builtin: &[adapters::ProviderAdapterEnum],
) -> Result<adapters::ProviderAdapterEnum, CoreError> {
    // 1. Built-in adapter?
    if let Some(a) = builtin.iter().find(|a| a.id() == provider_id) {
        return Ok(adapters::ProviderAdapterEnum::clone(a));
    }
    // 2. Custom provider in DB → build adapter on-the-fly.
    // `core_providers::get` is a SELECT — use the READER so this lookup
    // doesn't serialize through the writer mutex (chat hot path).
    let r = s.db_pool().reader();
    let provider_row = core_providers::get(&r, provider_id)
        .map_err(|e| CoreError::ProviderNotFound(format!("{provider_id}: {e}")))?;
    drop(r);
    match provider_row {
        Some(row) => Ok(adapters::ProviderAdapterEnum::Custom(
            adapters::CustomAdapter::from_provider_row(&row),
        )),
        None => Err(CoreError::ProviderNotFound(provider_id.to_string())),
    }
}
