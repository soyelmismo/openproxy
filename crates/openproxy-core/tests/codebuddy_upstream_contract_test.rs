//! Contract and integration tests for OpenProxy CodeBuddy (@tencent-ai/codebuddy-code) provider.
//!
//! Validates:
//! 1. 1:1 parity of the static 35-model catalog against product.json.
//! 2. Spoofer headers, User-Agent formatting, and dynamic overrides.
//! 3. Business error code mappings (6000-6008 rate limits, 14014/14018 credits exhausted, etc.).
//! 4. Database seeding of the built-in "codebuddy" provider.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use openproxy_adapters::adapters::codebuddy::{
    CodeBuddyAdapter, CodeBuddyErrorCode, NPM_CODEBUDDY_METADATA_URL,
    apply_codebuddy_spoofing_headers, codebuddy_static_models, get_codebuddy_version,
    parse_codebuddy_error_code, refresh_codebuddy_version, set_codebuddy_ua, set_codebuddy_version,
};
use openproxy_adapters::spoofer::{
    CODEBUDDY_ASYNC_TEST_LOCK, CODEBUDDY_TEST_LOCK, ClientSpoofer, CodeBuddySpoofer,
    current_codebuddy_ua, current_codebuddy_version, reset_dynamic_codebuddy_overrides,
    set_dynamic_codebuddy_extra_header, set_dynamic_codebuddy_ua, set_dynamic_codebuddy_version,
};
use openproxy_adapters::upstream::{TimeoutProfile, UpstreamClient};
use openproxy_adapters::{
    AdapterAuthType, AdapterFormat, ProviderAdapter, UpstreamRequest, builtin_adapters,
};
use openproxy_core::providers;
use openproxy_core::seed::{is_builtin, seed_builtin_providers};
use openproxy_db::conn::DbPool;
use openproxy_types::{ModelId, ProviderId, TargetFormat, UpstreamErrorClass};
use std::path::Path;

#[test]
fn test_codebuddy_spoofer_verified_headers() {
    let _guard = CODEBUDDY_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let _async_guard = CODEBUDDY_ASYNC_TEST_LOCK.blocking_lock();
    reset_dynamic_codebuddy_overrides();

    let mut req = UpstreamRequest::get("https://www.workbuddy.ai/v2/chat/completions");
    apply_codebuddy_spoofing_headers(&mut req);

    let get_header = |key: &str| {
        req.headers
            .get(key)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    };

    assert_eq!(get_header("x-ide-type"), Some("CLI".into()));
    assert_eq!(get_header("x-ide-name"), Some("CLI".into()));
    assert_eq!(get_header("x-ide-version"), Some("2.168.2".into()));
    assert_eq!(get_header("x-product"), Some("SaaS".into()));
    assert_eq!(get_header("x-agent-intent"), Some("craft".into()));
    assert_eq!(get_header("x-codebuddy-request"), Some("1".into()));
    assert_eq!(get_header("x-domain"), Some("www.workbuddy.ai".into()));
    assert_eq!(
        get_header("user-agent"),
        Some("CLI/2.168.2 CodeBuddy/2.168.2".into())
    );
}

#[test]
fn test_codebuddy_dynamic_spoofer_overrides() {
    let _guard = CODEBUDDY_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let _async_guard = CODEBUDDY_ASYNC_TEST_LOCK.blocking_lock();
    reset_dynamic_codebuddy_overrides();

    assert_eq!(current_codebuddy_version(), "2.168.2");
    assert_eq!(current_codebuddy_ua(), "CLI/2.168.2 CodeBuddy/2.168.2");

    set_dynamic_codebuddy_version("2.160.0");
    set_dynamic_codebuddy_extra_header("x-codebuddy-tenant", "tencent-cloud");

    let headers = CodeBuddySpoofer.headers();
    let find = |key: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(find("x-ide-version"), Some("2.160.0"));
    assert_eq!(find("user-agent"), Some("CLI/2.160.0 CodeBuddy/2.160.0"));
    assert_eq!(find("x-codebuddy-tenant"), Some("tencent-cloud"));

    set_dynamic_codebuddy_ua("CustomCodeBuddy/5.0");
    assert_eq!(current_codebuddy_ua(), "CustomCodeBuddy/5.0");
    assert_eq!(
        CodeBuddySpoofer
            .headers()
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
            .map(|(_, v)| v.as_str()),
        Some("CustomCodeBuddy/5.0")
    );

    set_codebuddy_ua("CustomCodeBuddy/6.0");
    assert_eq!(current_codebuddy_ua(), "CustomCodeBuddy/6.0");

    set_codebuddy_version("2.170.0".into());
    assert_eq!(get_codebuddy_version(), "2.170.0");
    assert_eq!(current_codebuddy_version(), "2.170.0");

    reset_dynamic_codebuddy_overrides();
    assert_eq!(current_codebuddy_version(), "2.168.2");
    assert_eq!(get_codebuddy_version(), "2.168.2");
    assert_eq!(current_codebuddy_ua(), "CLI/2.168.2 CodeBuddy/2.168.2");
}

#[test]
fn test_codebuddy_adapter_spec_parity() {
    let adapter = CodeBuddyAdapter::new();
    assert_eq!(adapter.id().as_str(), "codebuddy");
    assert_eq!(adapter.config().name, "CodeBuddy");
    assert_eq!(adapter.config().base_url, "https://www.workbuddy.ai/v2");
    assert_eq!(adapter.format(), AdapterFormat::Openai);
    assert_eq!(adapter.auth_type(), AdapterAuthType::OAuth);

    let chat_url = adapter.build_chat_url(TargetFormat::Openai, &ModelId::new("gpt-5.5"));
    assert_eq!(chat_url, "https://www.workbuddy.ai/v2/chat/completions");

    // models_url MUST be None to avoid 404 from upstream
    assert!(adapter.models_url().is_none());

    // canonical IDs for models.dev
    assert_eq!(
        adapter.models_dev_canonical_ids(),
        &["codebuddy", "tencent"]
    );

    let headers =
        adapter.build_headers("my-api-key", TargetFormat::Openai, &ModelId::new("gpt-5.5"));
    let find = |k: &str| {
        headers
            .iter()
            .find(|(hk, _)| hk.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(find("Authorization"), Some("Bearer my-api-key"));
    assert_eq!(find("x-ide-type"), Some("CLI"));
    assert_eq!(find("x-codebuddy-request"), Some("1"));
}

#[tokio::test]
async fn test_codebuddy_static_models_and_product_json_parity() {
    let models = codebuddy_static_models();
    assert_eq!(models.len(), 35, "Must have exactly 35 models");

    let gpt55 = models
        .iter()
        .find(|m| m.model_id.as_str() == "gpt-5.5")
        .expect("gpt-5.5");
    assert_eq!(gpt55.context_length, Some(1_000_000));
    assert_eq!(gpt55.max_output_tokens, Some(72_000));

    let minimax_m3 = models
        .iter()
        .find(|m| m.model_id.as_str() == "minimax-m3")
        .expect("minimax-m3");
    assert_eq!(minimax_m3.context_length, Some(512_000));
    assert_eq!(minimax_m3.max_output_tokens, Some(128_000));

    let hy3 = models
        .iter()
        .find(|m| m.model_id.as_str() == "hy3:free")
        .expect("hy3:free");
    assert_eq!(hy3.context_length, Some(192_000));

    let kimi_k3 = models
        .iter()
        .find(|m| m.model_id.as_str() == "kimi-k3")
        .expect("kimi-k3");
    assert_eq!(kimi_k3.context_length, Some(1_000_000));

    let local_product = Path::new("/tmp/codebuddy_inspect/package/product.json");
    if local_product.exists() {
        let content = std::fs::read_to_string(local_product).expect("read product.json");
        let parsed: serde_json::Value = serde_json::from_str(&content).expect("parse json");
        let upstream_models = parsed["models"].as_array().expect("models array");
        assert_eq!(
            upstream_models.len(),
            35,
            "product.json model count must match"
        );

        for um in upstream_models {
            let mid = um["id"].as_str().expect("model id");
            let dm = models
                .iter()
                .find(|m| m.model_id.as_str() == mid)
                .unwrap_or_else(|| panic!("Model {mid} missing from static catalog"));

            if let Some(expected_in) = um.get("maxInputTokens").and_then(|v| v.as_i64()) {
                assert_eq!(
                    dm.context_length,
                    Some(expected_in),
                    "context_length mismatch for {mid}"
                );
            }
            if let Some(expected_out) = um.get("maxOutputTokens").and_then(|v| v.as_i64()) {
                assert_eq!(
                    dm.max_output_tokens,
                    Some(expected_out),
                    "max_output_tokens mismatch for {mid}"
                );
            }
        }
    }
}

#[test]
fn test_codebuddy_error_mappings() {
    for code in 6000..=6008 {
        let parsed = CodeBuddyErrorCode::from_code(code).unwrap();
        assert!(parsed.is_rate_limit());
        assert_eq!(
            parsed.to_upstream_error_class(),
            UpstreamErrorClass::ResourceExhausted
        );
    }

    for code in [14001, 14002, 14012, 14013, 14014, 14018, 14019] {
        let parsed = CodeBuddyErrorCode::from_code(code).unwrap();
        assert!(parsed.is_credits_exhausted());
        assert_eq!(
            parsed.to_upstream_error_class(),
            UpstreamErrorClass::ResourceExhausted
        );
    }

    let expired = CodeBuddyErrorCode::from_code(14015).unwrap();
    assert!(expired.is_auth_error());
    assert_eq!(
        expired.to_upstream_error_class(),
        UpstreamErrorClass::PermissionDenied
    );

    let json_body = r#"{"code": 14014, "message": "Enterprise credits exhausted"}"#;
    assert_eq!(parse_codebuddy_error_code(json_body), Some(14014));

    // nested JSON-RPC shell error where outer code is -32603 and data has real code
    let nested_rpc =
        r#"{"status": 400, "error": {"code": -32603, "data": {"code": 14018, "statusCode": 403}}}"#;
    assert_eq!(parse_codebuddy_error_code(nested_rpc), Some(14018));

    let generic_400 = r#"{"error": {"code": 400, "message": "Bad Request"}}"#;
    assert_eq!(parse_codebuddy_error_code(generic_400), None);
}

#[test]
fn test_codebuddy_builtin_seed_and_registration() {
    assert!(
        is_builtin("codebuddy"),
        "codebuddy must be recognized as builtin"
    );

    let pool = DbPool::test_pool_with_prefix("openproxy-codebuddy-test").expect("open pool");
    let conn = pool.writer();

    let seeded = seed_builtin_providers(&conn).expect("seed");
    assert_eq!(seeded, 24, "should seed all 24 built-in providers");

    let cb = providers::get(&conn, &ProviderId::new("codebuddy"))
        .expect("get")
        .expect("codebuddy row exists");

    assert_eq!(cb.id.as_str(), "codebuddy");
    assert_eq!(&*cb.name, "CodeBuddy");
    assert_eq!(&*cb.base_url, "https://www.workbuddy.ai/v2");
    assert_eq!(cb.auth_type, openproxy_core::providers::AuthType::OAuth);
    assert_eq!(cb.format, openproxy_core::providers::ProviderFormat::Openai);

    let builtins = builtin_adapters();
    assert!(
        builtins
            .iter()
            .any(|a| a.config().id.as_str() == "codebuddy"),
        "codebuddy must be registered in builtin_adapters"
    );
}

#[tokio::test]
async fn test_codebuddy_remote_upstream_live_contract_parity() {
    let _lock = CODEBUDDY_ASYNC_TEST_LOCK.lock().await;
    let client = std::sync::Arc::new(UpstreamClient::new());

    let npm_url = NPM_CODEBUDDY_METADATA_URL;
    let cancel = openproxy_adapters::upstream::CancellationToken::new();
    let req = UpstreamRequest::get(npm_url);
    if let Ok(resp) = client.call(req, TimeoutProfile::OAuth, cancel).await {
        if resp.status.is_success() {
            if let Ok(body) = resp.collect().await
                && let Ok(pkg) = serde_json::from_slice::<serde_json::Value>(&body)
            {
                assert_eq!(pkg["name"], "@tencent-ai/codebuddy-code");
                assert!(
                    pkg["bin"]["codebuddy"].is_string()
                        || pkg["bin"]["cbc"].is_string()
                        || pkg["bin"]["codebuddy-code"].is_string(),
                    "Upstream package must expose codebuddy CLI binaries"
                );
                if let Some(ver) = pkg["version"].as_str() {
                    assert!(
                        ver.split('.').count() >= 3,
                        "NPM codebuddy version must follow semver X.Y.Z: {ver}"
                    );
                }
            }
        } else {
            eprintln!(
                "[CodeBuddyLiveTest] NPM registry returned HTTP {}, skipping registry check",
                resp.status
            );
        }
    } else {
        eprintln!("[CodeBuddyLiveTest] Offline or NPM unreachable, skipping registry check");
    }

    reset_dynamic_codebuddy_overrides();

    assert_eq!(current_codebuddy_version(), "2.168.2");
    assert_eq!(get_codebuddy_version(), "2.168.2");

    // El refresh actualiza en caliente los overrides dinámicos de versión si hay
    // respuesta del registro npm, manteniendo el header del spoofer alineado.
    let elevated = refresh_codebuddy_version(&client).await;
    assert!(
        elevated.is_some(),
        "refresh_codebuddy_version must resolve an elevated version from npm"
    );
    let elevated_ver = elevated.unwrap();
    assert_eq!(current_codebuddy_version(), elevated_ver.as_str());
    assert_eq!(get_codebuddy_version(), elevated_ver.as_str());

    reset_dynamic_codebuddy_overrides();
    assert_eq!(current_codebuddy_version(), "2.168.2");
}

#[tokio::test]
async fn test_codebuddy_fetch_models_does_not_elevate_the_version_by_itself() {
    let _lock = CODEBUDDY_ASYNC_TEST_LOCK.lock().await;
    reset_dynamic_codebuddy_overrides();

    assert_eq!(get_codebuddy_version(), "2.168.2");

    // Discovery es puramente local: el catálogo estático no consulta el
    // registro, así que ninguna metadata puede elevar la versión por sí sola.
    let client = std::sync::Arc::new(UpstreamClient::new());
    let adapter = CodeBuddyAdapter::new();

    let models = adapter
        .fetch_models(&client, "dummy-key")
        .await
        .expect("fetch models");
    let expected_catalog =
        openproxy_adapters::client_contract::providers::codebuddy::CodeBuddyCatalog::parse(
            include_str!("../../../fixtures/codebuddy-extracted/product.json"),
        )
        .expect("catálogo embebido válido");
    assert_eq!(models.len(), expected_catalog.models.len());
    for (actual, expected) in models.iter().zip(&expected_catalog.models) {
        let is_free = openproxy_adapters::adapters::codebuddy::is_codebuddy_model_free(
            expected.credits.as_deref(),
        );
        let expected_id = openproxy_adapters::adapters::codebuddy::make_codebuddy_model_id(
            expected.id.clone(),
            is_free,
        );
        assert_eq!(actual.model_id.as_str(), expected_id);
    }

    // Ningún bump en background: el cambio de versión llega solo por el
    // contrato verificado (ContractRefreshService + store compartido).
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    assert_eq!(get_codebuddy_version(), "2.168.2");

    reset_dynamic_codebuddy_overrides();
    assert_eq!(get_codebuddy_version(), "2.168.2");
}

#[test]
fn test_codebuddy_pinned_unverified_contract_preserves_header_identity() {
    let _guard = openproxy_adapters::spoofer::CODEBUDDY_TEST_LOCK
        .lock()
        .unwrap();
    reset_dynamic_codebuddy_overrides();
    let snapshot =
        openproxy_adapters::client_contract::providers::codebuddy_state::embedded_snapshot();
    assert!(!snapshot.payload.identity_activatable);
    assert!(snapshot.client_identity.version.is_none());
    let version = current_codebuddy_version();
    let adapter = CodeBuddyAdapter::new().with_pinned_contract(std::sync::Arc::new(snapshot));

    let headers = adapter.build_headers("key", TargetFormat::Openai, &ModelId::new("gpt-5.5"));
    let find = |k: &str| {
        headers
            .iter()
            .find(|(hk, _)| hk.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(find("x-ide-version"), Some(version.as_str()));
    assert_eq!(
        find("user-agent"),
        Some(format!("CLI/{version} CodeBuddy/{version}").as_str())
    );
    assert_eq!(find("x-ide-type"), Some("CLI"));
    assert_eq!(find("x-codebuddy-request"), Some("1"));
}
