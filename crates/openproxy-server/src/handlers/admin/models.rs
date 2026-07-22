use super::*;
use axum::{
    Json,
    extract::{Path, Query, State},
};

use openproxy_core::admin as core_admin;

pub async fn toggle_model(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    crate::api_try! {
        let active = body
            .get("active")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| CoreError::Validation("missing 'active' bool".into()))?;
        let w = s.db_pool().writer();
        core_models::set_active(&w, ModelRowId(id), active)?;
        Ok(Json(serde_json::json!({ "id": id, "active": active })))
    }
}

pub async fn bulk_toggle_models(
    State(s): State<AppState>,
    Json(body): Json<core_admin::BulkToggleInput>,
) -> ApiResult<Json<serde_json::Value>> {
    crate::api_try! {
        let w = s.db_pool().writer();
        let updated = core_admin::set_active_bulk(&w, body)?;
        Ok(Json(serde_json::json!({
            "updated": updated,
        })))
    }
}

pub async fn delete_model(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    crate::api_try! {
        let w = s.db_pool().writer();
        let removed = core_models::delete(&w, ModelRowId(id))?;
        Ok(Json(serde_json::json!({ "id": id, "deleted": removed })))
    }
}

pub async fn create_custom_model(
    State(s): State<AppState>,
    Json(input): Json<core_admin::CreateCustomModelInput>,
) -> ApiResult<Json<serde_json::Value>> {
    crate::api_try! {
        let w = s.db_pool().writer();
        let row_id = core_admin::create_custom_model(&w, input)?;
        Ok(Json(serde_json::json!({ "row_id": row_id.0 })))
    }
}

pub async fn test_model(
    State(s): State<AppState>,
    Path(model_row_id): Path<i64>,
    cancel_watch: Option<axum::Extension<crate::disconnect::CancelWatch>>,
    body_bytes: axum::body::Bytes,
) -> ApiResult<Json<serde_json::Value>> {
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
                return ApiResult::err(ApiError(CoreError::Parse(format!("Invalid JSON: {}", e))));
            }
        }
    };

    let r = run_test_for_model(
        &s,
        model_row_id,
        account_id,
        proxy_url,
        TestOptions::default(),
        cancel_rx,
    )
    .await;
    ApiResult::ok(Json(serde_json::json!({
        "row_id": r.row_id,
        "status": r.status,
        "elapsed_ms": r.elapsed_ms,
        "error_msg": r.error_msg,
    })))
}

pub async fn list_models_admin(
    State(s): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<ListModelsQuery>,
) -> ApiResult<Json<Vec<core_models::Model>>> {
    crate::api_try! {
        // Read-only SELECT — use the READER.
        let r = s.db_pool().reader();
        let mut list = core_models::list_all(&r)?;
        if let Some(p) = q.provider_id {
            list.retain(|m| m.provider_id.as_str() == p);
        }
        Ok(Json(list))
    }
}

pub async fn sync_models_dev(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let upstream = s.upstream_client().clone();
    let db_pool = s.db_pool().clone();
    let result = openproxy_core::models_dev_sync::run_one_shot(db_pool, upstream).await;
    let msg = match result {
        Ok(m) => m,
        Err(e) => return ApiResult::err(ApiError(e)),
    };
    ApiResult::ok(Json(serde_json::json!({ "message": msg })))
}

pub async fn refresh_models(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<RefreshQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    run_refresh(s, id, q).await
}

pub(crate) async fn run_test_for_model(
    s: &AppState,
    model_row_id: i64,
    account_id: Option<AccountId>,
    proxy_url: Option<String>,
    opts: TestOptions,
    cancel_rx: Option<tokio::sync::watch::Receiver<bool>>,
) -> TestResult {
    use openproxy_pipeline::translation::{openai_to_anthropic, openai_to_gemini};
    use openproxy_types::{OpenAIMessage, OpenAIRequest};

    let row_id = ModelRowId(model_row_id);
    let start = std::time::Instant::now();

    // 1. Load the model row.
    let model = match tokio::task::block_in_place(|| {
        let w = s.db_pool().writer();
        core_models::get_by_row_id(&w, row_id)?.ok_or_else(|| {
            ApiError(CoreError::ModelNotFound {
                provider: "<unknown>".into(),
                model: format!("row_id={}", model_row_id),
            })
        })
    }) {
        Ok(m) => m,
        Err(ApiError(e)) => {
            return TestResult {
                row_id: model_row_id,
                status: e.http_status(),
                elapsed_ms: 0,
                error_msg: Some(openproxy_core::cost::redact_error_msg(&e.to_string()).0),
                skipped: true,
                skip_reason: Some(format!(
                    "model lookup failed: {}",
                    openproxy_core::cost::redact_error_msg(&e.to_string()).0
                )),
            };
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
        return TestResult::skipped(model_row_id, "model is inactive");
    }

    // 2. Find the adapter for that provider. Check built-in adapters
    //    first, then fall back to constructing a CustomAdapter from the
    //    DB row.
    let adapter = match resolve_adapter(s, &model.provider_id, s.adapters().as_slice()) {
        Ok(a) => a.clone(),
        Err(err) => {
            return TestResult {
                row_id: model_row_id,
                status: err.http_status(),
                elapsed_ms: 0,
                error_msg: Some(openproxy_core::cost::redact_error_msg(&err.to_string()).0),
                skipped: true,
                skip_reason: Some(openproxy_core::cost::redact_error_msg(&err.to_string()).0),
            };
        }
    };

    // 3. Resolve the account to use. Anonymous access is allowed when:
    //      - provider has auth_type "none", OR
    //      - provider has no accounts configured (fallback to anonymous)
    //    This lets bearer providers like opencode-zen work without
    //    accounts while still using accounts when they exist.
    let (is_anonymous, accounts_list) = tokio::task::block_in_place(|| {
        let w = s.db_pool().writer();
        let provider_row = core_providers::get(&w, &model.provider_id).unwrap_or_default();
        let accs = core_accounts::list(&w, Some(&model.provider_id), s.master_key().as_ref())
            .unwrap_or_default();
        let anon = match &provider_row {
            Some(p) if matches!(p.auth_type, core_providers::AuthType::None) => true,
            _ if accs.is_empty() => true, // No accounts → try anonymous
            _ => false,
        };
        (anon, accs)
    });

    // Capture the optional account_id AND its label. The label is
    // needed by providers whose URL embeds account-level metadata
    // (e.g. CloudFlare Workers AI uses the label as its account ID).
    let mut raw_account_opt = None;
    let (_account_id_opt, _account_label, api_key) = if is_anonymous {
        (None, String::new(), String::new()) // Anonymous: no account, empty key
    } else {
        let selected = match account_id {
            Some(id) => {
                // Per-model path: look up the already-pinned account.
                let w = s.db_pool().writer();
                tokio::task::block_in_place(|| {
                    core_accounts::get(&w, id, s.master_key().as_ref())
                        .ok()
                        .flatten()
                })
            }
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
                    .cloned()
            }
        };

        let account_id = selected.as_ref().map(|a| a.id);
        let account_label = selected
            .as_ref()
            .and_then(|a| a.label.as_deref())
            .unwrap_or("")
            .to_string();

        // 4. Decrypt the API key. Drop the writer guard immediately.
        //    OAuth accounts store the token in access_token_encrypted,
        //    not api_key_encrypted, so we fall back to that if the
        //    primary decrypt fails (e.g. NULL column).
        let api_key = match account_id {
            Some(aid) => {
                let account = tokio::task::block_in_place(|| {
                    let w = s.db_pool().writer();
                    core_accounts::get(&w, aid, s.master_key().as_ref())
                        .ok()
                        .flatten()
                });
                raw_account_opt = account.clone();
                if let Some(ref acc) = account
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
                            let err_msg = format!("resolve oauth token: {}", e);
                            return TestResult {
                                row_id: model_row_id,
                                status: e.http_status(),
                                elapsed_ms,
                                error_msg: Some(err_msg),
                                skipped: false,
                                skip_reason: None,
                            };
                        }
                    }
                } else {
                    match tokio::task::block_in_place(|| {
                        let w = s.db_pool().writer();
                        core_accounts::decrypt_api_key(&w, aid, s.master_key().as_ref()).or_else(|_| {
                            core_accounts::decrypt_access_token(&w, aid, s.master_key().as_ref())
                        })
                    }).map_err(ApiError)
                    {
                        Ok(k) => k,
                        Err(ApiError(e)) => {
                            return TestResult {
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
                            };
                        }
                    }
                }
            }
            None => String::new(),
        };

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
        messages: vec![
            OpenAIMessage {
                role: "system".into(),
                content: Some(serde_json::Value::String(
                    "You are a helpful assistant.".to_string(),
                )),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
            OpenAIMessage {
                role: "user".into(),
                content: Some(serde_json::Value::String("ping".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
        ],
        stream: false,
        temperature: None,
        max_tokens: Some(5),
        top_p: None,
        stop: None,
        tools: None,
        tool_choice: None,
        top_k: None,
        user: None,
        extra: serde_json::Map::new(),
    };

    // 6. Custom providers (kiro) are not supported by the simple test path yet.
    // antigravity is supported via wrap_request_body.
    let is_custom_provider = matches!(model.provider_id.as_str(), "kiro");

    if is_custom_provider {
        return TestResult {
            row_id: model_row_id,
            status: 501,
            elapsed_ms: 0,
            error_msg: Some("Test not supported for custom providers yet".into()),
            skipped: true,
            skip_reason: Some("Test not supported for custom providers yet".into()),
        };
    }

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
            &_account_label,
        );
        match serde_json::to_value(&anthropic_req) {
            Ok(v) => (url, v),
            Err(e) => {
                let err = CoreError::Internal(format!("serialize anthropic req: {}", e));
                return TestResult {
                    row_id: model_row_id,
                    status: 500,
                    elapsed_ms: 0,
                    error_msg: Some(openproxy_core::cost::redact_error_msg(&err.to_string()).0),
                    skipped: true,
                    skip_reason: Some(openproxy_core::cost::redact_error_msg(&err.to_string()).0),
                };
            }
        }
    } else if effective_target_format == openproxy_core::models::TargetFormat::Gemini {
        let gemini_req = openai_to_gemini(&openai_req, &openai_req.messages);
        let url = adapter.build_chat_url_for_account(
            openproxy_core::models::TargetFormat::Gemini,
            &model.model_id,
            &_account_label,
        );
        match serde_json::to_value(&gemini_req) {
            Ok(v) => (url, v),
            Err(e) => {
                let err = CoreError::Internal(format!("serialize gemini req: {}", e));
                return TestResult {
                    row_id: model_row_id,
                    status: 500,
                    elapsed_ms: 0,
                    error_msg: Some(openproxy_core::cost::redact_error_msg(&err.to_string()).0),
                    skipped: true,
                    skip_reason: Some(openproxy_core::cost::redact_error_msg(&err.to_string()).0),
                };
            }
        }
    } else if effective_target_format == openproxy_core::models::TargetFormat::Responses {
        let url = adapter.build_chat_url_for_account(
            openproxy_core::models::TargetFormat::Responses,
            &model.model_id,
            &_account_label,
        );
        let mut responses_req = openai_req.clone();
        responses_req.max_tokens = None;
        let (_cancel_tx, client_disconnected) = tokio::sync::watch::channel(false);
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
        };
        let formatter = openproxy_pipeline::formatting::get_formatter(
            openproxy_core::models::TargetFormat::Responses,
        );
        match formatter.format_request(&pipeline_req, &model, &openai_req.messages, true, &adapter)
        {
            Ok(req_bytes) => match serde_json::from_slice::<serde_json::Value>(&req_bytes) {
                Ok(v) => (url, v),
                Err(e) => {
                    let err = CoreError::Internal(format!("serialize responses req: {}", e));
                    return TestResult {
                        row_id: model_row_id,
                        status: 500,
                        elapsed_ms: 0,
                        error_msg: Some(openproxy_core::cost::redact_error_msg(&err.to_string()).0),
                        skipped: true,
                        skip_reason: Some(
                            openproxy_core::cost::redact_error_msg(&err.to_string()).0,
                        ),
                    };
                }
            },
            Err(e) => {
                return TestResult {
                    row_id: model_row_id,
                    status: 500,
                    elapsed_ms: 0,
                    error_msg: Some(openproxy_core::cost::redact_error_msg(&e.to_string()).0),
                    skipped: true,
                    skip_reason: Some(openproxy_core::cost::redact_error_msg(&e.to_string()).0),
                };
            }
        }
    } else {
        let url = adapter.build_chat_url_for_account(
            openproxy_core::models::TargetFormat::Openai,
            &model.model_id,
            &_account_label,
        );
        match serde_json::to_value(&openai_req) {
            Ok(v) => (url, v),
            Err(e) => {
                let err = CoreError::Internal(format!("serialize openai req: {}", e));
                return TestResult {
                    row_id: model_row_id,
                    status: 500,
                    elapsed_ms: 0,
                    error_msg: Some(openproxy_core::cost::redact_error_msg(&err.to_string()).0),
                    skipped: true,
                    skip_reason: Some(openproxy_core::cost::redact_error_msg(&err.to_string()).0),
                };
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
                    .or_else(|| v.get("projectId"))
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
            rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
        },
        model: model.clone(),
        api_key: api_key.clone(),
        api_key_label: Some(_account_label.clone()),
        custom_meta,
    };

    let mut req = openproxy_adapters::upstream::UpstreamRequest::post_json(
        url,
        match serde_json::to_vec(&body_value) {
            Ok(b) => {
                match adapter.wrap_request_body(bytes::Bytes::from(b), effective_target_format, &model.model_id, &dummy_target) {
                    Ok(wrapped) => wrapped,
                    Err(e) => {
                        let _ = cancel_rx;
                        return TestResult {
                            row_id: model_row_id,
                            status: 500,
                            elapsed_ms: 0,
                            error_msg: Some(openproxy_core::cost::redact_error_msg(&format!("failed to wrap request: {}", e)).0),
                            skipped: true,
                            skip_reason: Some(openproxy_core::cost::redact_error_msg(&format!("failed to wrap request: {}", e)).0),
                        };
                    }
                }
            },
            Err(e) => {
                let _ = cancel_rx;
                return TestResult {
                    row_id: model_row_id,
                    status: 500,
                    elapsed_ms: 0,
                    error_msg: Some(
                        openproxy_core::cost::redact_error_msg(&format!(
                            "failed to serialize request: {}",
                            e
                        ))
                        .0,
                    ),
                    skipped: true,
                    skip_reason: Some(
                        openproxy_core::cost::redact_error_msg(&format!(
                            "failed to serialize request: {}",
                            e
                        ))
                        .0,
                    ),
                };
            }
        },
    );
    req.proxy = proxy_url.clone();
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

    if let Some(mut rx) = cancel_rx.clone() {
        let rx_cancel = cancel.clone();
        tokio::spawn(async move {
            if *rx.borrow() {
                rx_cancel.cancel();
                return;
            }
            while rx.changed().await.is_ok() {
                if *rx.borrow() {
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

    let result = client.call(req, profile, cancel).await;
    let elapsed_ms = start.elapsed().as_millis() as u64;

    let (status, error_msg) = match result {
        Ok(response) => {
            let status = response.status.as_u16();
            if status >= 400 {
                let body = response.collect().await.unwrap_or_default();
                let text = String::from_utf8_lossy(&body);
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
            (0, Some(format!("{:?}", e)))
        }
    };

    // 10. Persist the result. The persist is independent of the response
    //     shape: the dashboard should always see *something* on the row
    //     after the button is pressed. We write to the row from the
    //     per-row path only; the combo fan-out does not want its
    //     transient probe to overwrite the row's last-test status.
    if !opts.in_combo_fanout {
        let status_i32 = status as i32;
        if let Err(e) = tokio::task::block_in_place(|| {
            let w = s.db_pool().writer();
            core_models::set_test_status(&w, row_id, status_i32)
        }) {
            return TestResult {
                row_id: model_row_id,
                status: e.http_status(),
                elapsed_ms,
                error_msg: Some(openproxy_core::cost::redact_error_msg(&e.to_string()).0),
                skipped: true,
                skip_reason: Some(openproxy_core::cost::redact_error_msg(&e.to_string()).0),
            };
        }
    }

    TestResult {
        row_id: model_row_id,
        status,
        elapsed_ms,
        error_msg,
        skipped: false,
        skip_reason: None,
    }
}

pub(crate) async fn run_refresh(
    s: AppState,
    id: i64,
    q: RefreshQuery,
) -> ApiResult<Json<serde_json::Value>> {
    let row_id = ModelRowId(id);
    let ttl_seconds = q.ttl_seconds.unwrap_or(3_600);

    // 1. Look up the model to find the provider.
    let provider_id = {
        let w = s.db_pool().writer();
        let found = match core_models::get_by_row_id(&w, row_id) {
            Ok(opt) => opt,
            Err(e) => return ApiResult::err(ApiError(e)),
        };
        match found {
            Some(m) => m.provider_id,
            None => {
                return ApiResult::err(ApiError(CoreError::ModelNotFound {
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
        Ok(a) => a.clone(),
        Err(e) => return ApiResult::err(ApiError(e)),
    };

    // 3. Resolve an account and decrypt/refresh its credential.
    let selected_account_id = {
        let w = s.db_pool().writer();

        let provider_row = match core_providers::get(&w, &provider_id) {
            Ok(p) => p,
            Err(e) => return ApiResult::err(ApiError(e)),
        };
        let accounts_list =
            match core_accounts::list(&w, Some(&provider_id), s.master_key().as_ref()) {
                Ok(l) => l,
                Err(e) => return ApiResult::err(ApiError(e)),
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
                        return ApiResult::err(ApiError(CoreError::AccountNotFound(account_id.0)));
                    }
                    Err(e) => return ApiResult::err(ApiError(e)),
                }
            };
            if account.auth_type == "oauth" {
                refresh_oauth_if_needed(&s, account, &provider_id).await
            } else {
                let w = s.db_pool().writer();
                match core_accounts::decrypt_api_key(&w, account_id, s.master_key().as_ref()) {
                    Ok(k) => k,
                    Err(e) => return ApiResult::err(ApiError(e)),
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
        Err(e) => return ApiResult::err(ApiError(e)),
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
        Err(e) => return ApiResult::err(ApiError(e)),
    };

    ApiResult::ok(Json(serde_json::json!({
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
        return Ok(a.clone());
    }
    // 2. Custom provider in DB → build adapter on-the-fly.
    // `core_providers::get` is a SELECT — use the READER so this lookup
    // doesn't serialize through the writer mutex (chat hot path).
    let r = s.db_pool().reader();
    let provider_row = core_providers::get(&r, provider_id)
        .map_err(|e| CoreError::ProviderNotFound(format!("{}: {}", provider_id, e)))?;
    drop(r);
    match provider_row {
        Some(row) => Ok(adapters::ProviderAdapterEnum::Custom(
            adapters::CustomAdapter::from_provider_row(&row),
        )),
        None => Err(CoreError::ProviderNotFound(provider_id.to_string())),
    }
}
