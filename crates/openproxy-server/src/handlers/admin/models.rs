use super::{
    AccountId, ApiError, AppState, Arc, ComboId, CoreError, Deserialize, ModelRowId, ProviderId,
    RequestId, TraceId, adapters, core_accounts, core_models, core_oauth, core_providers,
};
use axum::{
    Json,
    extract::{Path, Query, State},
};

use openproxy_core::admin as core_admin;

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/", axum::routing::get(list_models_admin))
        .route("/custom", axum::routing::post(create_custom_model))
        .route("/bulk-toggle", axum::routing::post(bulk_toggle_models))
        .route("/sync-models-dev", axum::routing::post(sync_models_dev))
        .route("/{id}/refresh", axum::routing::post(refresh_models))
        .route("/{id}/toggle", axum::routing::post(toggle_model))
        .route(
            "/{id}/test",
            axum::routing::post(test_model).route_layer(axum::middleware::from_fn(
                crate::disconnect::client_disconnect_middleware,
            )),
        )
        .route(
            "/{id}",
            axum::routing::delete(delete_model).patch(update_model),
        )
}

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

crate::admin_entity_action_handler! {
    pub async fn delete_model(
        State(s) with writer(w),
        Path(id): Path<i64>,
    ) -> Result<Json<serde_json::Value>, ApiError> {
        let removed = core_models::delete(&w, ModelRowId(id))?;
        Ok(Json(serde_json::json!({ "id": id, "deleted": removed })))
    }
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

fn resolve_proxy_url_by_id(s: &AppState, pid: &str) -> Option<String> {
    tokio::task::block_in_place(|| {
        let r = s.db_pool().reader();
        let p = openproxy_core::free_proxies::get_proxy(&r, pid)
            .ok()
            .flatten()?;
        Some(format!(
            "{}://{}:{}",
            p.r#type.to_lowercase(),
            p.host,
            p.port
        ))
    })
}

fn parse_test_model_params(
    s: &AppState,
    body_bytes: &[u8],
) -> Result<(Option<AccountId>, Option<String>), ApiError> {
    if body_bytes.is_empty() {
        return Ok((None, None));
    }
    let input = serde_json::from_slice::<TestModelInput>(body_bytes)
        .map_err(|e| ApiError(CoreError::Parse(format!("Invalid JSON: {e}"))))?;
    let aid = input.account_id.map(AccountId::new);
    let purl = input
        .proxy_id
        .as_deref()
        .and_then(|pid| resolve_proxy_url_by_id(s, pid));
    Ok((aid, purl))
}

pub async fn test_model(
    State(s): State<AppState>,
    Path(model_row_id): Path<i64>,
    cancel_watch: Option<axum::Extension<crate::disconnect::CancelWatch>>,
    body_bytes: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    let cancel_rx = cancel_watch.map(|axum::Extension(cw)| cw.rx);
    let (account_id, proxy_url) = parse_test_model_params(&s, &body_bytes)?;

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

fn test_error_result(row_id: i64, status: u16, err_msg: &str) -> TestResult {
    let redacted = openproxy_core::cost::redact_error_msg(err_msg).0;
    TestResult {
        row_id,
        status,
        elapsed_ms: 0,
        error_msg: Some(redacted.clone()),
        skipped: true,
        skip_reason: Some(redacted),
    }
}

fn load_model_for_test(s: &AppState, model_row_id: i64) -> Result<core_models::Model, TestResult> {
    let r = s.db_pool().reader();
    match core_models::get_by_row_id(&r, ModelRowId(model_row_id)) {
        Ok(Some(m)) => Ok(m),
        Ok(None) => Err(test_error_result(
            model_row_id,
            404,
            &format!("model lookup failed: row_id={model_row_id}"),
        )),
        Err(e) => Err(test_error_result(
            model_row_id,
            e.http_status(),
            &format!("model lookup failed: {e}"),
        )),
    }
}

fn resolve_effective_target_format(
    format: adapters::AdapterFormat,
    fallback: openproxy_core::models::TargetFormat,
) -> openproxy_core::models::TargetFormat {
    match format {
        adapters::AdapterFormat::Openai => openproxy_core::models::TargetFormat::Openai,
        adapters::AdapterFormat::Anthropic => openproxy_core::models::TargetFormat::Anthropic,
        adapters::AdapterFormat::Mixed => fallback,
        adapters::AdapterFormat::Gemini => openproxy_core::models::TargetFormat::Gemini,
        adapters::AdapterFormat::Responses => openproxy_core::models::TargetFormat::Responses,
        adapters::AdapterFormat::Atomesus => openproxy_core::models::TargetFormat::Atomesus,
        adapters::AdapterFormat::Fx => openproxy_core::models::TargetFormat::Fx,
    }
}

fn select_account_candidate(accounts_list: &[core_accounts::Account]) -> Option<AccountId> {
    accounts_list
        .iter()
        .find(|a| a.health_status == core_accounts::HealthStatus::Healthy)
        .or_else(|| {
            accounts_list
                .iter()
                .find(|a| a.health_status == core_accounts::HealthStatus::Degraded)
        })
        .or_else(|| accounts_list.first())
        .map(|a| a.id)
}

async fn decrypt_test_account_key(
    s: &AppState,
    model_row_id: i64,
    aid: AccountId,
    account_opt: Option<&core_accounts::Account>,
    provider_id: &str,
    start: std::time::Instant,
) -> Result<String, TestResult> {
    if let Some(acc) = account_opt
        && acc.auth_type.as_ref() == "oauth"
    {
        return core_oauth::resolve_oauth_token(
            s.db_pool().as_ref(),
            acc,
            provider_id,
            s.oauth_provider_registry().as_ref(),
            s.upstream_client(),
            s.master_key().as_ref(),
        )
        .await
        .map_err(|e| {
            let elapsed_ms = start.elapsed().as_millis() as u64;
            TestResult {
                row_id: model_row_id,
                status: e.http_status(),
                elapsed_ms,
                error_msg: Some(format!("resolve oauth token: {e}")),
                skipped: false,
                skip_reason: None,
            }
        });
    }

    let r = s.db_pool().reader();
    core_accounts::decrypt_api_key(&r, aid, s.master_key().as_ref())
        .or_else(|_| core_accounts::decrypt_access_token(&r, aid, s.master_key().as_ref()))
        .map_err(|e| test_error_result(model_row_id, e.http_status(), &e.to_string()))
}

async fn resolve_test_credentials(
    s: &AppState,
    model: &core_models::Model,
    model_row_id: i64,
    account_id: Option<AccountId>,
    start: std::time::Instant,
) -> Result<
    (
        Option<AccountId>,
        String,
        String,
        Option<core_accounts::Account>,
    ),
    TestResult,
> {
    let (is_anonymous, accounts_list) = {
        let r = s.db_pool().reader();
        let provider_row = core_providers::get(&r, &model.provider_id).unwrap_or_default();
        let accs = core_accounts::list(&r, Some(&model.provider_id), s.master_key().as_ref())
            .unwrap_or_default();
        let anon = match &provider_row {
            Some(p) if matches!(p.auth_type, core_providers::AuthType::None) => true,
            _ if accs.is_empty() => true,
            _ => false,
        };
        (anon, accs)
    };

    if is_anonymous {
        return Ok((None, String::new(), String::new(), None));
    }

    let resolved_aid = account_id.or_else(|| select_account_candidate(&accounts_list));
    let raw_account = resolved_aid.and_then(|aid| {
        tokio::task::block_in_place(|| {
            let r = s.db_pool().reader();
            core_accounts::get(&r, aid, s.master_key().as_ref())
                .ok()
                .flatten()
        })
    });

    let api_key = match resolved_aid {
        Some(aid) => {
            decrypt_test_account_key(
                s,
                model_row_id,
                aid,
                raw_account.as_ref(),
                model.provider_id.as_str(),
                start,
            )
            .await?
        }
        None => String::new(),
    };

    let account_label = raw_account
        .as_ref()
        .and_then(|a| a.label.as_deref())
        .unwrap_or_default()
        .to_string();

    Ok((resolved_aid, account_label, api_key, raw_account))
}

fn build_stt_test_payload(
    adapter: &adapters::ProviderAdapterEnum,
    model: &core_models::Model,
) -> (String, serde_json::Value, Option<(String, bytes::Bytes)>) {
    let audio_wav = openproxy_core::audio::generate_test_speech_wav();
    let boundary = format!("----WebKitFormBoundary{}", uuid::Uuid::new_v4().simple());
    let mut payload = Vec::new();

    payload.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    payload.extend_from_slice(b"Content-Disposition: form-data; name=\"model\"\r\n\r\n");
    payload.extend_from_slice(model.model_id.as_str().as_bytes());
    payload.extend_from_slice(b"\r\n");

    payload.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    payload.extend_from_slice(
        b"Content-Disposition: form-data; name=\"response_format\"\r\n\r\njson\r\n",
    );

    payload.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    payload.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"hello.wav\"\r\n",
    );
    payload.extend_from_slice(b"Content-Type: audio/wav\r\n\r\n");
    payload.extend_from_slice(&audio_wav);
    payload.extend_from_slice(b"\r\n");
    payload.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

    let content_type = format!("multipart/form-data; boundary={boundary}");
    let url = adapter.build_transcription_url();
    let debug_val = serde_json::json!({
        "model": model.model_id.as_str(),
        "file": "hello.wav (16kHz 16-bit mono PCM speech)",
        "response_format": "json"
    });
    (
        url,
        debug_val,
        Some((content_type, bytes::Bytes::from(payload))),
    )
}

fn build_audio_or_specialized_payload(
    adapter: &adapters::ProviderAdapterEnum,
    model: &core_models::Model,
    is_embedding: bool,
    is_image: bool,
    _is_tts: bool,
) -> (String, serde_json::Value) {
    if is_embedding {
        let url = adapter.build_embeddings_url();
        let val = serde_json::json!({
            "model": model.model_id.as_str(),
            "input": "hello"
        });
        (url, val)
    } else if is_image {
        let url = adapter.build_image_url();
        let val = serde_json::json!({
            "model": model.model_id.as_str(),
            "prompt": "hello",
            "n": 1,
            "size": "256x256"
        });
        (url, val)
    } else {
        let base_url = adapter.config().base_url.as_str();
        let url = format!("{base_url}/audio/speech");
        let val = serde_json::json!({
            "model": model.model_id.as_str(),
            "input": "hello",
            "voice": "alloy"
        });
        (url, val)
    }
}

fn build_chat_format_test_payload(
    adapter: &adapters::ProviderAdapterEnum,
    model: &core_models::Model,
    openai_req: &openproxy_types::OpenAIRequest,
    account_label: &str,
    effective_target_format: openproxy_core::models::TargetFormat,
    model_row_id: i64,
) -> Result<(String, serde_json::Value), TestResult> {
    use openproxy_adapters::adapters::gemini::openai_to_gemini;
    use openproxy_pipeline::translation::openai_to_anthropic;

    let url =
        adapter.build_chat_url_for_account(effective_target_format, &model.model_id, account_label);

    match effective_target_format {
        openproxy_core::models::TargetFormat::Anthropic => {
            let anthropic_req = openai_to_anthropic(
                openai_req,
                model.model_id.as_str(),
                &openai_req.messages,
                openai_req.stream,
            );
            serde_json::to_value(&anthropic_req)
                .map(|v| (url, v))
                .map_err(|e| {
                    test_error_result(model_row_id, 500, &format!("serialize anthropic req: {e}"))
                })
        }
        openproxy_core::models::TargetFormat::Gemini => {
            let gemini_req = openai_to_gemini(openai_req, &openai_req.messages);
            serde_json::to_value(&gemini_req)
                .map(|v| (url, v))
                .map_err(|e| {
                    test_error_result(model_row_id, 500, &format!("serialize gemini req: {e}"))
                })
        }
        openproxy_core::models::TargetFormat::Responses => {
            let mut responses_req = openai_req.clone();
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
            let req_bytes = formatter
                .format_request(
                    &pipeline_req,
                    model,
                    &pipeline_req.openai_request.messages,
                    true,
                    adapter,
                )
                .map_err(|err| test_error_result(model_row_id, 500, &err.to_string()))?;
            let v = serde_json::from_slice::<serde_json::Value>(&req_bytes).map_err(|e| {
                test_error_result(model_row_id, 500, &format!("serialize responses req: {e}"))
            })?;
            Ok((url, v))
        }
        openproxy_core::models::TargetFormat::Fx => {
            let req_bytes = adapter
                .format_request(
                    openproxy_core::models::TargetFormat::Fx,
                    openai_req,
                    &model.model_id,
                    &openai_req.messages,
                    openai_req.stream,
                )
                .map_err(|err| test_error_result(model_row_id, 500, &err.to_string()))?;
            let v = serde_json::from_slice::<serde_json::Value>(&req_bytes).map_err(|e| {
                test_error_result(model_row_id, 500, &format!("serialize fx req: {e}"))
            })?;
            Ok((url, v))
        }
        _ => serde_json::to_value(openai_req)
            .map(|v| (url, v))
            .map_err(|e| {
                test_error_result(model_row_id, 500, &format!("serialize openai req: {e}"))
            }),
    }
}

fn extract_kiro_meta(
    raw_account: Option<&core_accounts::Account>,
) -> (Option<String>, Option<String>) {
    raw_account
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
        })
}

fn build_custom_provider_meta(
    provider_id: &str,
    raw_account_opt: Option<&core_accounts::Account>,
    api_key: &str,
) -> Option<openproxy_types::context::CustomProviderMeta> {
    if provider_id == "antigravity" {
        let antigravity_project = raw_account_opt
            .and_then(|a| a.oauth_provider_specific.as_deref())
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .and_then(|v| {
                openproxy_pipeline::credentials::antigravity_project_from_value(&v)
            });
        Some(openproxy_types::context::CustomProviderMeta {
            access_token: api_key.to_string(),
            maybe_refresh: None,
            kiro_region: None,
            kiro_profile_arn: None,
            antigravity_project,
            antigravity_metadata: None,
            codex_workspace_id: None,
        })
    } else if provider_id == "kiro" {
        let (region, profile_arn) = extract_kiro_meta(raw_account_opt);
        Some(openproxy_types::context::CustomProviderMeta {
            access_token: api_key.to_string(),
            maybe_refresh: None,
            kiro_region: region,
            kiro_profile_arn: profile_arn,
            antigravity_project: None,
            antigravity_metadata: None,
            codex_workspace_id: None,
        })
    } else {
        None
    }
}

fn build_test_openai_request(model_id: &str) -> openproxy_types::OpenAIRequest {
    openproxy_types::OpenAIRequest {
        model: model_id.to_string(),
        messages: vec![openproxy_types::OpenAIMessage {
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
    }
}

pub(crate) async fn run_test_for_model(
    s: &AppState,
    model_row_id: i64,
    account_id: Option<AccountId>,
    proxy_url: Option<String>,
    opts: TestOptions,
    cancel_rx: Option<tokio::sync::watch::Receiver<Option<openproxy_types::CancelReason>>>,
) -> (TestResult, Option<serde_json::Value>) {
    let row_id = ModelRowId(model_row_id);
    let start = std::time::Instant::now();

    // 1. Load model row
    let model = match load_model_for_test(s, model_row_id) {
        Ok(m) => m,
        Err(err_res) => return (err_res, None),
    };

    if !model.active && opts.in_combo_fanout {
        return (TestResult::skipped(model_row_id, "model is inactive"), None);
    }

    // 2. Resolve adapter
    let adapter = match resolve_adapter(s, &model.provider_id, s.adapters().as_slice()) {
        Ok(a) => a,
        Err(err) => {
            return (
                test_error_result(model_row_id, err.http_status(), &err.to_string()),
                None,
            );
        }
    };

    // 3. Resolve account & credentials
    let (_account_id_opt, account_label, api_key, raw_account_opt) =
        match resolve_test_credentials(s, &model, model_row_id, account_id, start).await {
            Ok(creds) => creds,
            Err(err_res) => return (err_res, None),
        };

    // 4. Build request
    let openai_req = build_test_openai_request(model.model_id.as_str());
    let effective_target_format =
        resolve_effective_target_format(adapter.format(), model.target_format);
    let inferred_type = openproxy_types::capabilities::infer_model_type(model.model_id.as_str());
    let is_audio = model.model_type.as_ref() == "audio" || inferred_type == "audio";
    let is_stt = is_audio
        && (openproxy_types::capabilities::is_stt_model(model.model_id.as_str())
            || model.model_type.as_ref() == "audio");
    let is_tts = is_audio && !is_stt;
    let is_embedding = model.model_type.as_ref() == "embedding" || inferred_type == "embedding";
    let is_image = model.model_type.as_ref() == "image" || inferred_type == "image";

    let (url, body_value, multipart_opt): (
        String,
        serde_json::Value,
        Option<(String, bytes::Bytes)>,
    ) = if is_stt {
        build_stt_test_payload(&adapter, &model)
    } else if is_embedding || is_image || is_tts {
        let (u, v) =
            build_audio_or_specialized_payload(&adapter, &model, is_embedding, is_image, is_tts);
        (u, v, None)
    } else {
        match build_chat_format_test_payload(
            &adapter,
            &model,
            &openai_req,
            &account_label,
            effective_target_format,
            model_row_id,
        ) {
            Ok((u, v)) => (u, v, None),
            Err(err_res) => return (err_res, None),
        }
    };

    let custom_meta = build_custom_provider_meta(
        model.provider_id.as_str(),
        raw_account_opt.as_ref(),
        &api_key,
    );
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

    let mut req = if let Some((content_type, body_bytes)) = multipart_opt {
        openproxy_adapters::upstream::UpstreamRequest::post_multipart(
            &url,
            &content_type,
            body_bytes,
        )
    } else {
        let wrapped_res = serde_json::to_vec(&body_value)
            .map_err(|e| format!("failed to serialize request: {e}"))
            .and_then(|b| {
                adapter
                    .wrap_request_body(
                        bytes::Bytes::from(b),
                        effective_target_format,
                        &dummy_target.model.model_id,
                        &dummy_target,
                    )
                    .map_err(|e| format!("failed to wrap request: {e}"))
            });

        match wrapped_res {
            Ok(wrapped) => openproxy_adapters::upstream::UpstreamRequest::post_json(&url, wrapped),
            Err(err_msg) => {
                return (test_error_result(model_row_id, 500, &err_msg), None);
            }
        }
    };
    req.proxy = proxy_url;
    for (k, v) in &headers {
        if is_stt && k.eq_ignore_ascii_case("content-type") {
            continue;
        }
        if let Ok(hn) = axum::http::HeaderName::from_bytes(k.as_bytes())
            && let Ok(hv) = axum::http::HeaderValue::from_str(v)
        {
            req.headers.insert(hn, hv);
        }
    }

    // 5. Upstream execution
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

    let start_req = std::time::Instant::now();
    let result = s.upstream_client().call(req, profile, cancel).await;
    let elapsed_ms = start_req.elapsed().as_millis() as u64;

    let mut debug_payload = request_headers_map.map(|req_headers| {
        serde_json::json!({
            "request_headers": req_headers,
            "request_url": url,
            "request_body": body_value,
        })
    });

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
        Err(e) => (0, Some(format!("{e:?}"))),
    };

    if !opts.in_combo_fanout {
        let status_i32 = i32::from(status);
        let w = s.db_pool().writer();
        if let Err(e) = core_models::set_test_status(&w, row_id, status_i32) {
            let mut err_res = test_error_result(model_row_id, e.http_status(), &e.to_string());
            err_res.elapsed_ms = elapsed_ms;
            return (err_res, None);
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
    let provider_id = {
        let r = s.db_pool().reader();
        let found = core_models::get_by_row_id(&r, row_id)?;
        match found {
            Some(m) => m.provider_id,
            None => {
                return Err(ApiError(CoreError::model_not_found(
                    "<unknown>",
                    format!("row_id={}", row_id.0),
                )));
            }
        }
    };

    let provider_q = super::providers::ProviderRefreshQuery {
        ttl_seconds: q.ttl_seconds,
        account_id: q.account_id,
    };
    super::providers::run_provider_refresh(s, provider_id.as_str(), provider_q).await
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
        Some(row) => Ok(adapters::ProviderAdapterEnum::Custom(Box::new(
            adapters::CustomAdapter::from_provider_row(&row),
        ))),
        None => Err(CoreError::ProviderNotFound(provider_id.to_string())),
    }
}
