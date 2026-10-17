use super::*;

#[test]
fn test_codebuddy_adapter_config() {
    let adapter = CodeBuddyAdapter::new();
    assert_eq!(adapter.id().as_str(), "codebuddy");
    assert_eq!(adapter.config().base_url, "https://www.codebuddy.ai/v2");
    assert_eq!(adapter.format(), AdapterFormat::Openai);
    assert_eq!(adapter.auth_type(), AdapterAuthType::OAuth);
    assert_eq!(
        adapter.build_chat_url(TargetFormat::Openai, &ModelId::new("gpt-5.5")),
        "https://www.codebuddy.ai/v2/chat/completions"
    );
    assert!(adapter.models_url().is_none());
}

#[test]
fn test_codebuddy_adapter_metadata_quota_support() {
    let adapter = CodeBuddyAdapter::new();
    let meta = adapter.metadata();
    assert!(meta.built_in);
    assert!(!meta.deletable);
    assert!(meta.supports_quota, "codebuddy must support quota tracking");
    assert!(
        meta.quota_refresh_supported,
        "codebuddy must support quota refresh"
    );
    assert!(meta.requires_oauth);
    assert_eq!(meta.oauth_refresh_lead_seconds, Some(300));
}

#[test]
fn test_codebuddy_static_models_count_and_content() {
    let models = codebuddy_static_models();
    assert_eq!(
        models.len(),
        35,
        "must contain exactly 35 models from product.json"
    );

    let find_model = |id: &str| models.iter().find(|m| m.model_id.as_str() == id);

    let gpt55 = find_model("gpt-5.5").expect("gpt-5.5 present");
    assert_eq!(gpt55.display_name.as_deref(), Some("GPT-5.5"));
    assert_eq!(gpt55.context_length, Some(1_000_000));
    assert_eq!(gpt55.max_output_tokens, Some(72_000));
    assert_eq!(gpt55.model_type.as_deref(), Some("chat"));
    let caps = gpt55.capabilities.as_ref().unwrap();
    assert_eq!(caps.vision, Some(true));
    assert_eq!(caps.tool_calling, Some(true));
    assert_eq!(caps.reasoning, Some(true));

    let minimax_m3 = find_model("minimax-m3").expect("minimax-m3 present");
    assert_eq!(minimax_m3.context_length, Some(512_000));
    assert_eq!(minimax_m3.max_output_tokens, Some(128_000));

    let hunyuan_video = find_model("hunyuan-video-art").expect("hunyuan-video-art present");
    assert_eq!(hunyuan_video.model_type.as_deref(), Some("video"));

    let gemini_image = find_model("gemini-3.0-pro-image").expect("gemini-3.0-pro-image present");
    assert_eq!(gemini_image.model_type.as_deref(), Some("image"));
}

#[test]
fn test_codebuddy_error_code_classification() {
    for code in 6000..=6008 {
        let err = CodeBuddyErrorCode::from_code(code).expect("valid code");
        assert!(err.is_rate_limit());
        assert_eq!(
            err.to_upstream_error_class(),
            openproxy_types::UpstreamErrorClass::ResourceExhausted
        );
    }

    let err_14014 = CodeBuddyErrorCode::from_code(14014).expect("14014 valid");
    assert!(err_14014.is_credits_exhausted());
    assert_eq!(err_14014.error_subcategory(), "quota_balance_exhausted");
    assert_eq!(
        err_14014.to_upstream_error_class(),
        openproxy_types::UpstreamErrorClass::ResourceExhausted
    );

    let err_14018 = CodeBuddyErrorCode::from_code(14018).expect("14018 valid");
    assert!(err_14018.is_credits_exhausted());
    assert_eq!(err_14018.error_subcategory(), "quota_balance_exhausted");

    let err_auth = CodeBuddyErrorCode::from_code(14015).expect("14015 valid");
    assert!(err_auth.is_auth_error());
    assert_eq!(
        err_auth.to_upstream_error_class(),
        openproxy_types::UpstreamErrorClass::PermissionDenied
    );

    let err_11101 = CodeBuddyErrorCode::from_code(11101).expect("11101 valid");
    assert_eq!(err_11101.error_subcategory(), "non_stream_not_supported");
    assert_eq!(
        err_11101.to_upstream_error_class(),
        openproxy_types::UpstreamErrorClass::InvalidPayload
    );

    let err_11128 = CodeBuddyErrorCode::from_code(11128).expect("11128 valid");
    assert_eq!(
        err_11128.error_subcategory(),
        "first_message_not_system_prompt"
    );
    assert_eq!(
        err_11128.to_upstream_error_class(),
        openproxy_types::UpstreamErrorClass::InvalidPayload
    );

    let body = r#"{"code": 14014, "message": "Enterprise usage exhausted"}"#;
    assert_eq!(parse_codebuddy_error_code(body), Some(14014));

    let body_11101 =
        r#"{"code": 11101, "msg": "Non-stream chat request is currently not supported"}"#;
    assert_eq!(parse_codebuddy_error_code(body_11101), Some(11101));

    let body_11128 = r#"{"code": 11128, "msg": "first message is not system prompt"}"#;
    assert_eq!(parse_codebuddy_error_code(body_11128), Some(11128));

    let body2 = r#"{"error": {"code": 6001, "message": "TPS limit exceeded"}}"#;
    assert_eq!(parse_codebuddy_error_code(body2), Some(6001));

    // Nested JSON-RPC shell where outer code is -32603 and inner business code is 11115 (ContextTooLong)
    let body_jsonrpc =
        r#"{"status": 400, "error": {"code": -32603, "data": {"code": 11115, "statusCode": 400}}}"#;
    assert_eq!(parse_codebuddy_error_code(body_jsonrpc), Some(11115));

    // HTTP status code 400 without business code is ignored
    let body_generic =
        r#"{"error": {"code": 400, "message": "Invalid argument: max_tokens 6000"}}"#;
    assert_eq!(parse_codebuddy_error_code(body_generic), None);
}

fn dummy_codebuddy_resolved_target() -> openproxy_types::context::ResolvedTarget {
    openproxy_types::context::ResolvedTarget {
        target: openproxy_types::combos::ComboTarget {
            id: openproxy_types::ComboTargetId(1),
            combo_id: openproxy_types::ComboId(1),
            provider_id: openproxy_types::ProviderId::new("codebuddy"),
            account_id: None,
            model_row_id: Some(openproxy_types::ModelRowId(1)),
            sub_combo_id: None,
            priority_order: 0,
            weight: 100,
            active: true,
            rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            thinking_effort: None,
            description: None,
            selected_accounts: None,
        },
        model: openproxy_types::Model {
            row_id: openproxy_types::ModelRowId(1),
            provider_id: openproxy_types::ProviderId::new("codebuddy"),
            target_format: openproxy_types::TargetFormat::Openai,
            discovered_at: openproxy_types::now_unix_secs_str().into_boxed_str(),
            expires_at: None,
            model_id: openproxy_types::ModelId::new("hy3"),
            display_name: None,
            context_length: None,
            max_output_tokens: None,
            model_type: "chat".into(),
            family: None,
            input_modalities_json: None,
            output_modalities_json: None,
            capabilities_json: None,
            timeout_overrides_json: None,
            active: true,
            last_test_status: None,
            last_test_at: None,
            custom: false,
            ..Default::default()
        },
        api_key: "test-token".to_string(),
        api_key_label: None,
        custom_meta: None,
    }
}

#[test]
fn test_codebuddy_forces_stream_in_normalize_and_wrap() {
    let adapter = CodeBuddyAdapter::new();

    // 1. normalize_openai_request sets stream = true
    let req = openproxy_types::OpenAIRequest {
        model: "hy3".into(),
        stream: false,
        ..Default::default()
    };
    let mut view = openproxy_types::OpenAIRequestView::new(&req, "hy3", &[], false);
    assert!(!view.stream);
    adapter.normalize_openai_request(&mut view);
    assert!(
        view.stream,
        "must force stream = true for CodeBuddy upstream"
    );

    // 2. wrap_request_body injects stream = true into json body
    let raw_unary_json =
        r#"{"model":"hy3","messages":[{"role":"user","content":"hello"}],"stream":false}"#;
    let resolved_target = dummy_codebuddy_resolved_target();
    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from(raw_unary_json),
            TargetFormat::Openai,
            &ModelId::new("hy3"),
            &resolved_target,
        )
        .expect("wrap succeeds");
    let wrapped_val: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();
    assert_eq!(
        wrapped_val
            .get("stream")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "wrapped body must enforce stream: true"
    );

    // 3. wrap_request_body guarantees first message is system prompt
    let messages = wrapped_val
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(
        messages[0].get("role").and_then(serde_json::Value::as_str),
        Some("system")
    );
    assert_eq!(
        messages[0]
            .get("content")
            .and_then(serde_json::Value::as_str),
        Some(DEFAULT_CODEBUDDY_SYSTEM_PROMPT)
    );
    assert_eq!(
        messages[1].get("role").and_then(serde_json::Value::as_str),
        Some("user")
    );
    assert_eq!(
        messages[1]
            .get("content")
            .and_then(serde_json::Value::as_str),
        Some("hello")
    );
}

#[test]
fn test_codebuddy_system_prompt_handling_scenarios() {
    let adapter = CodeBuddyAdapter::new();
    let resolved_target = dummy_codebuddy_resolved_target();

    // Scenario A: When index 0 is not system, default system prompt is prepended at index 0
    // and subsequent conversation history is kept intact to guarantee prefix KV cache invariance.
    let json_with_later_system = r#"{"model":"hy3","messages":[{"role":"user","content":"help"},{"role":"system","content":"custom instructions"}],"stream":true}"#;
    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from(json_with_later_system),
            TargetFormat::Openai,
            &ModelId::new("hy3"),
            &resolved_target,
        )
        .unwrap();
    let val: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();
    let msgs = val
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(
        msgs[0].get("role").and_then(serde_json::Value::as_str),
        Some("system")
    );
    assert_eq!(
        msgs[0].get("content").and_then(serde_json::Value::as_str),
        Some(DEFAULT_CODEBUDDY_SYSTEM_PROMPT)
    );
    assert_eq!(
        msgs[1].get("role").and_then(serde_json::Value::as_str),
        Some("user")
    );
    assert_eq!(
        msgs[2].get("role").and_then(serde_json::Value::as_str),
        Some("system")
    );
    assert_eq!(
        msgs[2].get("content").and_then(serde_json::Value::as_str),
        Some("custom instructions")
    );

    // Scenario B: Developer role is converted to system role at index 0
    let json_with_developer = r#"{"model":"hy3","messages":[{"role":"developer","content":"rules"},{"role":"user","content":"help"}],"stream":true}"#;
    let wrapped_dev = adapter
        .wrap_request_body(
            bytes::Bytes::from(json_with_developer),
            TargetFormat::Openai,
            &ModelId::new("hy3"),
            &resolved_target,
        )
        .unwrap();
    let val_dev: serde_json::Value = serde_json::from_slice(&wrapped_dev).unwrap();
    let msgs_dev = val_dev
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .unwrap();
    assert_eq!(
        msgs_dev[0].get("role").and_then(serde_json::Value::as_str),
        Some("system")
    );
    assert_eq!(
        msgs_dev[0]
            .get("content")
            .and_then(serde_json::Value::as_str),
        Some("rules")
    );

    // Scenario C: Empty messages array gets default system prompt
    let json_empty = r#"{"model":"hy3","messages":[],"stream":true}"#;
    let wrapped_empty = adapter
        .wrap_request_body(
            bytes::Bytes::from(json_empty),
            TargetFormat::Openai,
            &ModelId::new("hy3"),
            &resolved_target,
        )
        .unwrap();
    let val_empty: serde_json::Value = serde_json::from_slice(&wrapped_empty).unwrap();
    let msgs_empty = val_empty
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .unwrap();
    assert_eq!(msgs_empty.len(), 1);
    assert_eq!(
        msgs_empty[0]
            .get("role")
            .and_then(serde_json::Value::as_str),
        Some("system")
    );
}

#[test]
fn test_codebuddy_headers_generation() {
    let adapter = CodeBuddyAdapter::new();
    let headers = adapter.build_headers(
        "test-token-123",
        TargetFormat::Openai,
        &ModelId::new("gpt-5.5"),
    );
    let find = |key: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(find("Authorization"), Some("Bearer test-token-123"));
    assert_eq!(find("Content-Type"), Some("application/json"));
    assert_eq!(find("x-ide-type"), Some("CLI"));
    assert_eq!(find("x-ide-name"), Some("CLI"));
    assert_eq!(find("x-product"), Some("SaaS"));
    assert_eq!(find("x-agent-intent"), Some("craft"));
    assert_eq!(find("x-codebuddy-request"), Some("1"));
    assert!(find("user-agent").unwrap().contains("CodeBuddy"));
}

#[test]
fn test_codebuddy_dynamic_version_helpers() {
    let _guard = crate::spoofer::CODEBUDDY_TEST_LOCK.lock().unwrap();
    reset_dynamic_codebuddy_overrides();

    assert_eq!(get_codebuddy_version(), DEFAULT_CODEBUDDY_VERSION);
    assert_eq!(
        get_codebuddy_ua(),
        format!("CLI/{DEFAULT_CODEBUDDY_VERSION} CodeBuddy/{DEFAULT_CODEBUDDY_VERSION}")
    );

    set_codebuddy_version("2.160.0".into());
    assert_eq!(get_codebuddy_version(), "2.160.0");
    assert_eq!(get_codebuddy_ua(), "CLI/2.160.0 CodeBuddy/2.160.0");

    set_codebuddy_ua("CustomCodeBuddy/1.0");
    assert_eq!(get_codebuddy_ua(), "CustomCodeBuddy/1.0");

    reset_dynamic_codebuddy_overrides();
    assert_eq!(get_codebuddy_version(), DEFAULT_CODEBUDDY_VERSION);
    assert_eq!(
        get_codebuddy_ua(),
        format!("CLI/{DEFAULT_CODEBUDDY_VERSION} CodeBuddy/{DEFAULT_CODEBUDDY_VERSION}")
    );
}

#[test]
fn test_codebuddy_npm_metadata_url_override() {
    let _guard = crate::spoofer::CODEBUDDY_TEST_LOCK.lock().unwrap();
    let _async_guard = crate::spoofer::CODEBUDDY_ASYNC_TEST_LOCK.blocking_lock();
    unsafe {
        std::env::remove_var("OPENPROXY_CODEBUDDY_NPM_METADATA_URL");
    }
    assert_eq!(codebuddy_npm_metadata_url(), NPM_CODEBUDDY_METADATA_URL);
    unsafe {
        std::env::set_var(
            "OPENPROXY_CODEBUDDY_NPM_METADATA_URL",
            "https://custom-registry.local/@tencent-ai/codebuddy-code/latest",
        );
    }
    assert_eq!(
        codebuddy_npm_metadata_url(),
        "https://custom-registry.local/@tencent-ai/codebuddy-code/latest"
    );
    unsafe {
        std::env::remove_var("OPENPROXY_CODEBUDDY_NPM_METADATA_URL");
    }
    assert_eq!(codebuddy_npm_metadata_url(), NPM_CODEBUDDY_METADATA_URL);
}

use crate::client_contract::providers::codebuddy_runtime::{
    CODEBUDDY_REGISTRY_LATEST_URL, pinned_package_url, pinned_product_url,
};
use crate::client_contract::providers::codebuddy_state::CodeBuddyContractRuntime;
use crate::client_contract::{ContractError, ContractTransport, FetchRequest, FetchResponse};

const REFRESH_REGISTRY_JSON: &str =
    include_str!("../../../../../fixtures/codebuddy/registry-latest.json");
const REFRESH_PACKAGE_JSON: &str =
    include_str!("../../../../../fixtures/codebuddy-extracted/package.json");
const REFRESH_PRODUCT_JSON: &str =
    include_str!("../../../../../fixtures/codebuddy-extracted/product.json");

/// Transporte hermético del refresh: responde desde un mapa de URL y ninguna
/// petición sale del proceso. Una URL ausente se comporta como 404.
struct RefreshTransport {
    responses: std::sync::Mutex<std::collections::HashMap<String, FetchResponse>>,
}

impl RefreshTransport {
    fn with(entries: Vec<(String, FetchResponse)>) -> Self {
        Self {
            responses: std::sync::Mutex::new(entries.into_iter().collect()),
        }
    }
}

impl ContractTransport for RefreshTransport {
    fn fetch(
        &self,
        request: &FetchRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<FetchResponse, ContractError>>
                + Send
                + '_,
        >,
    > {
        let response = self
            .responses
            .lock()
            .unwrap()
            .get(&request.url)
            .cloned()
            .unwrap_or(FetchResponse {
                status: 404,
                headers: Vec::new(),
                body: Vec::new(),
            });
        Box::pin(async move { Ok(response) })
    }
}

fn refresh_ok_json(body: &[u8]) -> FetchResponse {
    FetchResponse {
        status: 200,
        headers: Vec::new(),
        body: body.to_vec(),
    }
}

fn refresh_temp_dir(name: &str) -> std::path::PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("openproxy-codebuddy-refresh-{name}-{stamp}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Ruta de refresh validada sobre runtime hermético: la metadata que anuncia
/// otra versión es rechazada y la generación activa se conserva.
#[tokio::test]
async fn test_codebuddy_refresh_rejects_metadata_only_elevation() {
    let dir = refresh_temp_dir("metadata-only");
    let runtime = CodeBuddyContractRuntime::new(&dir);
    let seed = runtime.load().expect("seed embebido");

    let transport = std::sync::Arc::new(RefreshTransport::with(vec![
        (
            CODEBUDDY_REGISTRY_LATEST_URL.to_string(),
            refresh_ok_json(br#"{"name":"@tencent-ai/codebuddy-code","version":"2.199.9"}"#),
        ),
        (
            pinned_package_url("2.199.9"),
            refresh_ok_json(br#"{"name":"@tencent-ai/codebuddy-code","version":"2.199.9"}"#),
        ),
        (pinned_product_url("2.199.9"), refresh_ok_json(b"{}")),
    ]));
    let elevated = runtime.refresh_validated(transport).await;
    assert_eq!(elevated, None, "metadata sola no eleva la identidad");
    assert_eq!(
        runtime.snapshot().unwrap().generation,
        seed.generation,
        "la generación activa no cambia"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// La ruta de refresh validada publica el catálogo como overlay, pero sin
/// correspondencia paquete→commit verificada la versión anunciada no sube.
#[tokio::test]
async fn test_codebuddy_refresh_pinned_round_does_not_elevate_the_version() {
    let dir = refresh_temp_dir("pinned-round");
    let runtime = CodeBuddyContractRuntime::new(&dir);
    let seed = runtime.load().expect("seed embebido");
    let version = seed.payload.version.clone();

    let transport = std::sync::Arc::new(RefreshTransport::with(vec![
        (
            CODEBUDDY_REGISTRY_LATEST_URL.to_string(),
            refresh_ok_json(REFRESH_REGISTRY_JSON.as_bytes()),
        ),
        (
            pinned_package_url(&version),
            refresh_ok_json(REFRESH_PACKAGE_JSON.as_bytes()),
        ),
        (
            pinned_product_url(&version),
            refresh_ok_json(REFRESH_PRODUCT_JSON.as_bytes()),
        ),
    ]));
    let elevated = runtime.refresh_validated(transport).await;
    assert_eq!(elevated, None, "sin gitHead no hay identidad activable");
    let snapshot = runtime.snapshot().unwrap();
    assert_eq!(snapshot.payload.version, version);
    assert!(!snapshot.payload.identity_activatable);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fallas de adquisición son fail-closed: `None`, sin pánico y sin cambiar la
/// generación activa.
#[tokio::test]
async fn test_codebuddy_refresh_failures_are_fail_closed() {
    let dir = refresh_temp_dir("failures");
    let runtime = CodeBuddyContractRuntime::new(&dir);
    let seed = runtime.load().expect("seed embebido");

    // 429 en el registro: error de adquisición, generación intacta.
    let throttled = std::sync::Arc::new(RefreshTransport::with(vec![(
        CODEBUDDY_REGISTRY_LATEST_URL.to_string(),
        FetchResponse {
            status: 429,
            headers: Vec::new(),
            body: Vec::new(),
        },
    )]));
    assert_eq!(runtime.refresh_validated(throttled).await, None);

    // 200 con un cuerpo que no es JSON: el parseo falla, generación intacta.
    let not_json = std::sync::Arc::new(RefreshTransport::with(vec![(
        CODEBUDDY_REGISTRY_LATEST_URL.to_string(),
        refresh_ok_json(b"NotJson"),
    )]));
    assert_eq!(runtime.refresh_validated(not_json).await, None);

    assert_eq!(runtime.snapshot().unwrap().generation, seed.generation);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_calculate_next_midnight_cst() {
    let ts = calculate_next_midnight_cst_unix_secs();
    let now = chrono::Utc::now().timestamp() as u64;
    assert!(ts > now, "next midnight CST must be in the future");
    assert!(
        ts <= now + 86_400 + 3_600,
        "next midnight CST must be within 25 hours"
    );
}

#[test]
fn test_parse_cst_datetime_to_unix_secs() {
    let ts = parse_cst_datetime_to_unix_secs("2026-09-30 23:59:59").expect("valid cst date");
    assert_eq!(ts, 1790783999);
}

#[test]
fn test_parse_codebuddy_resource_quota_with_bonus_and_free() {
    let val = serde_json::json!({
        "code": 0,
        "msg": "OK",
        "data": {
            "Response": {
                "Data": {
                    "TotalCount": 2,
                    "TotalDosage": 350,
                    "Accounts": [
                        {
                            "PackageCode": "TCACA_code_006_DbXS0lrypC",
                            "PackageName": "Bonus Pack",
                            "CapacitySize": 250,
                            "CapacityRemain": 250,
                            "CapacityUsed": 0,
                            "CycleStartTime": "2027-09-22 22:53:45",
                            "CycleEndTime": "2027-10-06 22:53:44",
                            "DeductionEndTime": 1822844024000_i64,
                            "Status": 0
                        },
                        {
                            "PackageCode": "TCACA_code_035_ArVxJcGDsm",
                            "PackageName": "Free Plan Subscription",
                            "CapacitySize": 100,
                            "CapacityRemain": 100,
                            "CapacityUsed": 0,
                            "CycleStartTime": "2027-09-01 00:00:00",
                            "CycleEndTime": "2027-09-30 23:59:59",
                            "DeductionEndTime": 2050412025000_i64,
                            "Status": 0
                        }
                    ]
                }
            }
        }
    });

    let quota = parse_codebuddy_resource_quota_at(&val, 1788000000).expect("parsed resource quota");
    assert_eq!(quota.session_limit, Some(350));
    assert_eq!(quota.session_used, Some(0));
    assert_eq!(
        quota.plan_name.as_deref(),
        Some("CodeBuddy: Bonus Pack (250 credits) + Free Plan Subscription (100 credits)")
    );
    assert!(
        quota.session_reset_at.is_none()
            || quota.session_reset_at.as_deref() == Some("1822319999")
            || quota.session_reset_at.as_deref() == Some("1822834424")
    );

    let details = quota.model_details.unwrap();
    // minimax-m3: 350 / 0.25 = 1400 calls
    let m3 = details.iter().find(|d| d.model_id == "minimax-m3").unwrap();
    assert_eq!(m3.session_limit, 1400);
    assert_eq!(m3.session_used, 0);
    assert_eq!(m3.remaining_fraction, 1.0);
}

#[test]
fn test_parse_codebuddy_resource_quota_summary_fallback() {
    let val = serde_json::json!({
        "code": 0,
        "msg": "OK",
        "data": {
            "Packages": [
                {
                    "PackageCode": "TCACA_code_006_DbXS0lrypC",
                    "CycleTotalCapacity": "250",
                    "CycleRemainCapacity": "250",
                    "CycleUsedCapacity": "0"
                },
                {
                    "PackageCode": "TCACA_code_035_ArVxJcGDsm",
                    "CycleTotalCapacity": "100",
                    "CycleRemainCapacity": "100",
                    "CycleUsedCapacity": "0"
                }
            ]
        }
    });

    let quota = parse_codebuddy_resource_quota(&val).expect("parsed summary quota");
    assert_eq!(quota.session_limit, Some(350));
    assert_eq!(quota.session_used, Some(0));
    assert_eq!(
        quota.plan_name.as_deref(),
        Some(
            "CodeBuddy: TCACA_code_006_DbXS0lrypC (250 credits) + TCACA_code_035_ArVxJcGDsm (100 credits)"
        )
    );
}

#[test]
fn test_parse_codebuddy_accounts_quota_default() {
    let val = serde_json::json!({
        "code": 0,
        "msg": "ok",
        "data": {
            "accounts": [
                {
                    "uid": "user_12345",
                    "type": "personal",
                    "pluginEnabled": true
                }
            ]
        }
    });

    let quota = parse_codebuddy_accounts_quota(&val);
    assert_eq!(quota.session_limit, Some(0));
    assert_eq!(quota.session_used, Some(0));
    assert_eq!(quota.plan_name.as_deref(), Some("CodeBuddy Account"));
    assert!(quota.session_reset_at.is_none());
    assert!(quota.model_details.is_some());
}

#[test]
fn test_build_codebuddy_resource_request_headers() {
    let req = build_codebuddy_resource_request(
        CODEBUDDY_GET_USER_RESOURCE_URL,
        "test-token-456",
        Some("http://proxy.local:8080"),
    );
    assert_eq!(req.proxy.as_deref(), Some("http://proxy.local:8080"));
    assert_eq!(
        req.headers
            .get(http::header::AUTHORIZATION)
            .unwrap()
            .to_str()
            .unwrap(),
        "Bearer test-token-456"
    );
    assert_eq!(
        req.headers
            .get(http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "application/json"
    );
    assert_eq!(
        req.headers.get("x-ide-type").unwrap().to_str().unwrap(),
        "CLI"
    );
}

#[test]
fn test_codebuddy_pinned_unverified_contract_does_not_elevate_headers() {
    let snapshot = crate::client_contract::providers::codebuddy_state::embedded_snapshot();
    assert!(!snapshot.payload.identity_activatable);
    assert!(snapshot.client_identity.version.is_none());
    let baseline = CodeBuddyAdapter::new().build_headers(
        "test-token",
        TargetFormat::Openai,
        &ModelId::new("gpt-5.5"),
    );
    let adapter = CodeBuddyAdapter::new().with_pinned_contract(Arc::new(snapshot));
    let headers =
        adapter.build_headers("test-token", TargetFormat::Openai, &ModelId::new("gpt-5.5"));
    assert_eq!(
        headers, baseline,
        "el catálogo no autoriza elevar identidad"
    );
}

#[test]
fn test_codebuddy_pinned_contract_drives_the_announced_headers() {
    // Snapshot sintético activable: comprueba solo el consumo del adapter.
    let mut snapshot = crate::client_contract::providers::codebuddy_state::embedded_snapshot();
    snapshot.payload.identity_activatable = true;
    snapshot.client_identity.version = Some("2.199.9".to_string());
    let version = snapshot.client_identity.version.clone().unwrap();
    let adapter = CodeBuddyAdapter::new().with_pinned_contract(Arc::new(snapshot));

    assert!(
        adapter.pinned_contract().is_some(),
        "el pin debe quedar congelado"
    );

    let headers =
        adapter.build_headers("test-token", TargetFormat::Openai, &ModelId::new("gpt-5.5"));
    let find = |key: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    };
    // Versión y user-agent salen del MISMO snapshot: una sola generación
    // coherente para toda la petición.
    assert_eq!(find("x-ide-version"), Some(version.as_str()));
    assert_eq!(
        find("user-agent"),
        Some(format!("CLI/{version} CodeBuddy/{version}").as_str())
    );
    // El resto de la identidad anunciada no cambia.
    assert_eq!(find("x-ide-type"), Some("CLI"));
    assert_eq!(find("x-codebuddy-request"), Some("1"));
}

#[tokio::test]
async fn test_codebuddy_discovery_reads_the_pinned_catalog_not_the_static_list() {
    let mut snapshot = crate::client_contract::providers::codebuddy_state::embedded_snapshot();
    let mut product: serde_json::Value =
        serde_json::from_slice(snapshot.catalog_body().unwrap()).unwrap();
    product["models"].as_array_mut().unwrap().truncate(1);
    let expected_id = product["models"][0]["id"].as_str().unwrap().to_string();
    let expected_input = product["models"][0]["maxInputTokens"].as_i64();
    snapshot.catalog = Some(Arc::from(serde_json::to_vec(&product).unwrap()));
    let adapter = CodeBuddyAdapter::new().with_pinned_contract(Arc::new(snapshot));
    let discovered = adapter
        .fetch_models(&UpstreamClient::new(), "unused")
        .await
        .unwrap();
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].model_id.as_str(), expected_id);
    assert_eq!(discovered[0].context_length, expected_input);
}

#[tokio::test]
async fn test_codebuddy_discovery_falls_back_when_snapshot_has_no_catalog() {
    let mut snapshot = crate::client_contract::providers::codebuddy_state::embedded_snapshot();
    snapshot.catalog = None;
    let adapter = CodeBuddyAdapter::new().with_pinned_contract(Arc::new(snapshot));
    let discovered = adapter
        .fetch_models(&UpstreamClient::new(), "unused")
        .await
        .unwrap();
    assert_eq!(discovered.len(), CODEBUDDY_MODELS.len());
}

#[test]
fn test_codebuddy_config_override_wins_over_the_pinned_contract() {
    let snapshot = crate::client_contract::providers::codebuddy_state::embedded_snapshot();
    let mut adapter = CodeBuddyAdapter::new().with_pinned_contract(Arc::new(snapshot));
    adapter
        .config_mut()
        .expect("config mutable")
        .extra_headers
        .push(("x-ide-version".to_string(), "9.9.9".to_string()));

    let headers =
        adapter.build_headers("test-token", TargetFormat::Openai, &ModelId::new("gpt-5.5"));
    let find = |key: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    };
    // El override explícito del despliegue gana sobre el contrato.
    assert_eq!(find("x-ide-version"), Some("9.9.9"));
}
