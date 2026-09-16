use super::super::{
    AccountId, AppState, ComboId, ModelRowId, RequestId, TraceId, adapters, core_accounts,
    core_models, core_oauth, core_providers,
};
use super::TestResult;

pub fn test_error_result(row_id: i64, status: u16, err_msg: &str) -> TestResult {
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

pub fn load_model_for_test(
    s: &AppState,
    model_row_id: i64,
) -> Result<core_models::Model, TestResult> {
    let r = s.db_pool().reader();
    match core_models::get_by_row_id(&r, ModelRowId(model_row_id)) {
        Ok(Some(mut m)) => {
            let inferred_type =
                openproxy_types::capabilities::infer_model_type(m.model_id.as_str());
            let effective_type = openproxy_types::capabilities::resolve_effective_model_type(
                &m.model_type,
                m.custom,
                inferred_type,
            );
            if m.model_type.as_ref() != effective_type {
                m.model_type = effective_type.to_string().into_boxed_str();
            }
            Ok(m)
        }
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

pub fn resolve_effective_target_format(
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
        adapters::AdapterFormat::CommandCodeGo => {
            openproxy_core::models::TargetFormat::CommandCodeGo
        }
    }
}

pub fn select_account_candidate(accounts_list: &[core_accounts::Account]) -> Option<AccountId> {
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

pub async fn decrypt_test_account_key(
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

pub async fn resolve_test_credentials(
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
    let raw_account = if let Some(aid) = resolved_aid {
        let pool = std::sync::Arc::clone(s.db_pool());
        let master_key = std::sync::Arc::clone(s.master_key());
        tokio::task::spawn_blocking(move || -> Option<_> {
            let r = pool.try_reader_for(std::time::Duration::from_secs(5))?;
            core_accounts::get(&r, aid, &master_key).ok().flatten()
        })
        .await
        .ok()
        .flatten()
    } else {
        None
    };

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

pub fn build_stt_test_payload(
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

pub fn build_audio_or_specialized_payload(
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

pub fn build_chat_format_test_payload(
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
                pii_session: std::sync::Arc::new(parking_lot::Mutex::new(None)),
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
        _ => serde_json::to_value(openai_req)
            .map(|v| (url, v))
            .map_err(|e| {
                test_error_result(model_row_id, 500, &format!("serialize openai req: {e}"))
            }),
    }
}

pub fn extract_kiro_meta(
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

pub fn build_custom_provider_meta(
    provider_id: &str,
    raw_account_opt: Option<&core_accounts::Account>,
    api_key: &str,
) -> Option<openproxy_types::context::CustomProviderMeta> {
    if provider_id == "antigravity" {
        let antigravity_project = raw_account_opt
            .and_then(|a| a.oauth_provider_specific.as_deref())
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .and_then(|v| openproxy_pipeline::credentials::antigravity_project_from_value(&v));
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

pub fn build_test_openai_request(model_id: &str) -> openproxy_types::OpenAIRequest {
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
