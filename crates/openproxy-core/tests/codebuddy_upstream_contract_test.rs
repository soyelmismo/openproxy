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
    reset_dynamic_codebuddy_overrides();

    let mut req = UpstreamRequest::get("https://www.codebuddy.ai/v2/chat/completions");
    apply_codebuddy_spoofing_headers(&mut req);

    let get_header = |key: &str| {
        req.headers
            .get(key)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    };

    assert_eq!(get_header("x-ide-type"), Some("CLI".into()));
    assert_eq!(get_header("x-ide-name"), Some("CLI".into()));
    assert_eq!(get_header("x-ide-version"), Some("2.156.0".into()));
    assert_eq!(get_header("x-product"), Some("SaaS".into()));
    assert_eq!(get_header("x-agent-intent"), Some("craft".into()));
    assert_eq!(get_header("x-codebuddy-request"), Some("1".into()));
    assert_eq!(
        get_header("user-agent"),
        Some("CLI/2.156.0 CodeBuddy/2.156.0".into())
    );
}

#[test]
fn test_codebuddy_dynamic_spoofer_overrides() {
    let _guard = CODEBUDDY_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    reset_dynamic_codebuddy_overrides();

    assert_eq!(current_codebuddy_version(), "2.156.0");
    assert_eq!(current_codebuddy_ua(), "CLI/2.156.0 CodeBuddy/2.156.0");

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
    assert_eq!(current_codebuddy_version(), "2.156.0");
    assert_eq!(get_codebuddy_version(), "2.156.0");
    assert_eq!(current_codebuddy_ua(), "CLI/2.156.0 CodeBuddy/2.156.0");
}

#[test]
fn test_codebuddy_adapter_spec_parity() {
    let adapter = CodeBuddyAdapter::new();
    assert_eq!(adapter.id().as_str(), "codebuddy");
    assert_eq!(adapter.config().name, "CodeBuddy");
    assert_eq!(adapter.config().base_url, "https://www.codebuddy.ai/v2");
    assert_eq!(adapter.format(), AdapterFormat::Openai);
    assert_eq!(adapter.auth_type(), AdapterAuthType::OAuth);

    let chat_url = adapter.build_chat_url(TargetFormat::Openai, &ModelId::new("gpt-5.5"));
    assert_eq!(chat_url, "https://www.codebuddy.ai/v2/chat/completions");

    // models_url MUST be None to avoid 404 from upstream
    assert!(adapter.models_url().is_none());

    // Canonical IDs for models.dev
    assert_eq!(
        adapter.models_dev_canonical_ids(),
        &["codebuddy", "tencent"]
    );

    // Headers include both auth and spoofing
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

    // Check key models
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
        .find(|m| m.model_id.as_str() == "hy3")
        .expect("hy3");
    assert_eq!(hy3.context_length, Some(192_000));

    let kimi_k3 = models
        .iter()
        .find(|m| m.model_id.as_str() == "kimi-k3")
        .expect("kimi-k3");
    assert_eq!(kimi_k3.context_length, Some(1_000_000));

    // Check parity against local product.json if available
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
    // 6000-6008 rate limits
    for code in 6000..=6008 {
        let parsed = CodeBuddyErrorCode::from_code(code).unwrap();
        assert!(parsed.is_rate_limit());
        assert_eq!(
            parsed.to_upstream_error_class(),
            UpstreamErrorClass::ResourceExhausted
        );
    }

    // Credits exhausted
    for code in [14001, 14002, 14012, 14013, 14014, 14018, 14019] {
        let parsed = CodeBuddyErrorCode::from_code(code).unwrap();
        assert!(parsed.is_credits_exhausted());
        assert_eq!(
            parsed.to_upstream_error_class(),
            UpstreamErrorClass::ResourceExhausted
        );
    }

    // Auth expired
    let expired = CodeBuddyErrorCode::from_code(14015).unwrap();
    assert!(expired.is_auth_error());
    assert_eq!(
        expired.to_upstream_error_class(),
        UpstreamErrorClass::PermissionDenied
    );

    // Parsing from json body
    let json_body = r#"{"code": 14014, "message": "Enterprise credits exhausted"}"#;
    assert_eq!(parse_codebuddy_error_code(json_body), Some(14014));

    // Nested JSON-RPC shell error where outer code is -32603 and data has real code
    let nested_rpc =
        r#"{"status": 400, "error": {"code": -32603, "data": {"code": 14018, "statusCode": 403}}}"#;
    assert_eq!(parse_codebuddy_error_code(nested_rpc), Some(14018));

    // HTTP status code 400 without business code
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
    assert_eq!(seeded, 22, "should seed all 22 built-in providers");

    let cb = providers::get(&conn, &ProviderId::new("codebuddy"))
        .expect("get")
        .expect("codebuddy row exists");

    assert_eq!(cb.id.as_str(), "codebuddy");
    assert_eq!(&*cb.name, "CodeBuddy");
    assert_eq!(&*cb.base_url, "https://www.codebuddy.ai/v2");
    assert_eq!(cb.auth_type, openproxy_core::providers::AuthType::OAuth);
    assert_eq!(cb.format, openproxy_core::providers::ProviderFormat::Openai);

    // Verify presence in builtin_adapters registry
    let builtins = builtin_adapters();
    assert!(
        builtins
            .iter()
            .any(|a| a.config().id.as_str() == "codebuddy"),
        "codebuddy must be registered in builtin_adapters"
    );
}

// ============================================================================
// 5. Remote Live Upstream Contract & In-Memory Auto-Update Verification
// ============================================================================

#[tokio::test]
async fn test_codebuddy_remote_upstream_live_contract_parity() {
    let client = std::sync::Arc::new(UpstreamClient::new());

    // 1. Probe official NPM package registry for CodeBuddy CLI metadata (@tencent-ai/codebuddy-code)
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

    // 2. Test in-memory auto-update via refresh_codebuddy_version
    let _lock = CODEBUDDY_ASYNC_TEST_LOCK.lock().await;
    reset_dynamic_codebuddy_overrides();

    assert_eq!(current_codebuddy_version(), "2.156.0");
    assert_eq!(get_codebuddy_version(), "2.156.0");

    if let Some(new_ver) = refresh_codebuddy_version(&client).await {
        assert_eq!(current_codebuddy_version(), new_ver);
        assert_eq!(get_codebuddy_version(), new_ver);
        assert_eq!(
            current_codebuddy_ua(),
            format!("CLI/{new_ver} CodeBuddy/{new_ver}")
        );

        let headers = CodeBuddySpoofer.headers();
        let find_hdr = |k: &str| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(
            find_hdr("user-agent"),
            Some(format!("CLI/{new_ver} CodeBuddy/{new_ver}").as_str())
        );
        assert_eq!(find_hdr("x-ide-version"), Some(new_ver.as_str()));
    } else {
        eprintln!(
            "[CodeBuddyLiveTest] Offline or NPM unreachable, skipping live auto-update check"
        );
    }

    reset_dynamic_codebuddy_overrides();
    assert_eq!(current_codebuddy_version(), "2.156.0");
}

#[tokio::test]
async fn test_codebuddy_fetch_models_triggers_background_auto_update() {
    let _lock = CODEBUDDY_ASYNC_TEST_LOCK.lock().await;
    reset_dynamic_codebuddy_overrides();

    assert_eq!(get_codebuddy_version(), "2.156.0");

    // Spin up an ephemeral local HTTP mock registry
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let body = r#"{"name":"@tencent-ai/codebuddy-code","version":"2.188.0"}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes()).await;
        }
    });

    let mock_url = format!("http://{addr}/@tencent-ai/codebuddy-code/latest");
    unsafe {
        std::env::set_var("OPENPROXY_CODEBUDDY_NPM_METADATA_URL", &mock_url);
    }

    let client = std::sync::Arc::new(UpstreamClient::new());
    let adapter = CodeBuddyAdapter::new();

    let models = adapter
        .fetch_models(&client, "dummy-key")
        .await
        .expect("fetch models");
    assert_eq!(models.len(), 35);

    // Give background task a moment to complete
    let mut ok = false;
    for _ in 0..100 {
        if get_codebuddy_version() == "2.188.0" {
            ok = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }
    assert!(ok, "background auto update must complete within 3s");

    unsafe {
        std::env::remove_var("OPENPROXY_CODEBUDDY_NPM_METADATA_URL");
    }

    assert_eq!(get_codebuddy_version(), "2.188.0");
    assert_eq!(current_codebuddy_ua(), "CLI/2.188.0 CodeBuddy/2.188.0");

    reset_dynamic_codebuddy_overrides();
    assert_eq!(get_codebuddy_version(), "2.156.0");
}
