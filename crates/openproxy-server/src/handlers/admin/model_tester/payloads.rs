use super::super::{
    AccountId, AppState, ModelRowId, adapters, core_accounts, core_models, core_oauth,
    core_providers,
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
        adapters::AdapterFormat::SystemOne => openproxy_core::models::TargetFormat::SystemOne,
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
    let (accounts_list, _provider_row) = {
        let r = s.db_pool().reader();
        let p = core_providers::get(&r, &model.provider_id).unwrap_or_default();
        let accs = core_accounts::list(&r, Some(&model.provider_id), s.master_key().as_ref())
            .unwrap_or_default();
        (accs, p)
    };

    let resolved_aid = account_id.or_else(|| select_account_candidate(&accounts_list));
    if let Some(aid) = resolved_aid {
        let pool = std::sync::Arc::clone(s.db_pool());
        let master_key = std::sync::Arc::clone(s.master_key());
        let raw_account = tokio::task::spawn_blocking(move || -> Option<_> {
            let r = pool.try_reader_for(std::time::Duration::from_secs(5))?;
            core_accounts::get(&r, aid, &master_key).ok().flatten()
        })
        .await
        .ok()
        .flatten();

        let api_key = decrypt_test_account_key(
            s,
            model_row_id,
            aid,
            raw_account.as_ref(),
            model.provider_id.as_str(),
            start,
        )
        .await?;

        let account_label = raw_account
            .as_ref()
            .and_then(|a| a.label.as_deref())
            .unwrap_or_default()
            .to_string();

        Ok((Some(aid), account_label, api_key, raw_account))
    } else {
        // No manual account selected, and no accounts exist in DB.
        // Fall back to keyless/anonymous request.
        Ok((None, String::new(), String::new(), None))
    }
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
    } else if provider_id == "codex" {
        let codex_workspace_id = raw_account_opt
            .and_then(|a| a.oauth_provider_specific.as_deref())
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .and_then(|meta| {
                meta.get("workspaceId")
                    .or_else(|| meta.get("workspace_id"))
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.is_empty())
                    .map(ToString::to_string)
            });
        Some(openproxy_types::context::CustomProviderMeta {
            access_token: api_key.to_string(),
            maybe_refresh: None,
            kiro_region: None,
            kiro_profile_arn: None,
            antigravity_project: None,
            antigravity_metadata: None,
            codex_workspace_id,
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
