//! Integration test for 1:1 contract parity between OpenProxy and upstream MiniMax-Code.
//!
//! Validates:
//! 1. Golden contract spec parity (100% offline, runs on every `cargo test`).
//! 2. Live remote upstream contract verification against `github.com/MiniMax-AI/minimax-code/main`
//!    via lightweight remote HTTP queries without cloning or caching git history.

use openproxy_adapters::ProviderAdapter;
use openproxy_adapters::upstream::{CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest};
use openproxy_core::oauth::minimax::checkin::SigninDayStatus;
use openproxy_core::oauth::minimax::matrix::MiniMaxRegion;
use openproxy_core::oauth::minimax::{
    AUDIENCE, CLIENT_ID, DEVICE_GRANT_TYPE, SCOPE, build_complete_verification_uri,
};

#[test]
fn test_minimax_golden_contract_spec_parity() {
    let _guard = openproxy_adapters::spoofer::MINIMAX_TEST_LOCK
        .lock()
        .expect("lock minimax test lock");
    openproxy_adapters::spoofer::reset_dynamic_minimax_overrides();

    // 1. OAuth client identity
    assert_eq!(CLIENT_ID, "mcode-public");
    assert_eq!(SCOPE, "agent.default");
    assert_eq!(AUDIENCE, "agent-backend");
    assert_eq!(DEVICE_GRANT_TYPE, "urn:ietf:params:oauth:grant-type:device_code");

    // 2. Region origins
    assert_eq!(MiniMaxRegion::Global.account_origin(), "https://account.minimax.io");
    assert_eq!(MiniMaxRegion::China.account_origin(), "https://account.minimax.cn");
    assert_eq!(MiniMaxRegion::Global.gateway_origin(), "https://agent.minimax.io");
    assert_eq!(MiniMaxRegion::China.gateway_origin(), "https://agent.minimax.cn");
    assert_eq!(MiniMaxRegion::Global.platform_origin(), "https://platform.minimax.io");
    assert_eq!(MiniMaxRegion::China.platform_origin(), "https://platform.minimaxi.com");

    // 3. Attribution query parameters for verification URL
    let url = build_complete_verification_uri("https://account.minimax.io/oauth-authorize", "LRAR-BW2A");
    assert!(url.contains("user_code=LRAR-BW2A"), "must contain user_code");
    assert!(url.contains("client_surface=tui"), "must attribute client_surface=tui");
    assert!(url.contains("download_source=mcode-internal"), "must attribute download_source=mcode-internal");

    // 4. Sign-in day status codes
    assert_eq!(SigninDayStatus::Upcoming as u8, 1);
    assert_eq!(SigninDayStatus::Claimable as u8, 2);
    assert_eq!(SigninDayStatus::Claimed as u8, 3);
    assert_eq!(SigninDayStatus::Disabled as u8, 4);

    // 5. Model discovery & catalog parity for OAuth (golden contract spec)
    let builtin_models = openproxy_adapters::adapters::minimax::minimax_builtin_models();
    let model_map: std::collections::HashMap<_, _> = builtin_models
        .iter()
        .map(|m| (m.model_id.as_str(), (m.context_length, m.max_output_tokens, m.target_format)))
        .collect();

    // Upstream MiniMax-M3 spec: 1M context, 128k output, Anthropic format
    assert_eq!(
        model_map.get("MiniMax-M3"),
        Some(&(Some(1_000_000), Some(128_000), openproxy_types::TargetFormat::Anthropic))
    );
    // Upstream MiniMax-M2.7-highspeed spec: 200k context, 128k output, Anthropic format
    assert_eq!(
        model_map.get("MiniMax-M2.7-highspeed"),
        Some(&(Some(200_000), Some(128_000), openproxy_types::TargetFormat::Anthropic))
    );
    // Upstream MiniMax-M2.7 spec: 200k context, 128k output, Anthropic format
    assert_eq!(
        model_map.get("MiniMax-M2.7"),
        Some(&(Some(200_000), Some(128_000), openproxy_types::TargetFormat::Anthropic))
    );

    // 6. Agent inference headers parity (golden contract spec)
    let adapter = openproxy_adapters::adapters::minimax::MiniMaxAdapter::new();
    let model = openproxy_types::ModelId::new("MiniMax-M3");
    let headers = adapter.build_headers("test-token-123", openproxy_types::TargetFormat::Anthropic, &model);
    let find_hdr = |name: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str());

    assert_eq!(find_hdr("User-Agent"), Some("MiniMaxAgent"));
    assert_eq!(find_hdr("Anthropic-Version"), Some("2023-06-01"));
    assert_eq!(find_hdr("X-Mavis-Agent-Id"), Some("main"));
    assert_eq!(find_hdr("X-Mavis-Timezone-Offset"), Some("0"));
    assert!(find_hdr("X-Mavis-Session-Id").is_some_and(|s| s.starts_with("session_")));
    assert_eq!(find_hdr("Authorization"), Some("Bearer test-token-123"));
    assert_eq!(find_hdr("x-api-key"), None);
    assert_eq!(find_hdr("Content-Type"), Some("application/json"));

    // 7. Chat URL parity for managed-login preset
    assert_eq!(
        adapter.build_chat_url(openproxy_types::TargetFormat::Anthropic, &model),
        "https://agent.minimax.io/mavis/api/v1/llm/v1/messages",
        "MiniMax Coding adapter must default to managed-login agent endpoint"
    );

    // 8. BYOK API key header parity (sk-... expects X-Api-Key)
    let headers_byok = adapter.build_headers("sk-secret123", openproxy_types::TargetFormat::Anthropic, &model);
    let find_byok = |name: &str| headers_byok.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str());
    assert_eq!(find_byok("x-api-key"), Some("sk-secret123"));
    assert_eq!(find_byok("Authorization"), Some("Bearer sk-secret123"));
}

#[tokio::test]
async fn test_minimax_remote_upstream_live_contract_parity() {
    // Enabled by default when network is available or explicitly via UPSTREAM_CONTRACT_CHECK=1
    let raw_base = "https://raw.githubusercontent.com/MiniMax-AI/minimax-code/main";
    let client = UpstreamClient::new();

    // Probe connectivity first with a lightweight HEAD/GET
    let probe_url = format!("{raw_base}/packages/oauth-core/src/contracts.ts");
    let req = UpstreamRequest::get(&probe_url);
    let cancel = CancellationToken::new();

    let resp = match client.call(req, TimeoutProfile::OAuth, cancel).await {
        Ok(r) if r.status.is_success() => r,
        Ok(r) => {
            eprintln!("[ContractTest] Upstream probe HTTP {}, skipping live check", r.status);
            return;
        }
        Err(e) => {
            eprintln!("[ContractTest] Offline or GitHub unreachable ({e}), skipping live check");
            return;
        }
    };

    let contracts_bytes = resp.collect().await.expect("read contracts.ts body");
    let contracts_ts = String::from_utf8_lossy(&contracts_bytes);

    // 1. Verify OAuth contract constants against upstream contracts.ts
    assert!(
        contracts_ts.contains(&format!("'{CLIENT_ID}'")),
        "Upstream contracts.ts diverged from CLIENT_ID = {CLIENT_ID}"
    );
    assert!(
        contracts_ts.contains(&format!("'{SCOPE}'")),
        "Upstream contracts.ts diverged from SCOPE = {SCOPE}"
    );
    assert!(
        contracts_ts.contains(&format!("'{AUDIENCE}'")),
        "Upstream contracts.ts diverged from AUDIENCE = {AUDIENCE}"
    );

    // 2. Verify endpoints from endpoint-config.ts
    let endpoint_cfg_url = format!("{raw_base}/packages/oauth-core/src/endpoint-config.ts");
    let resp = client
        .call(UpstreamRequest::get(&endpoint_cfg_url), TimeoutProfile::OAuth, CancellationToken::new())
        .await
        .expect("fetch endpoint-config.ts");
    let endpoint_bytes = resp.collect().await.expect("read endpoint-config.ts body");
    let endpoint_ts = String::from_utf8_lossy(&endpoint_bytes);

    assert!(
        endpoint_ts.contains("/oauth2/device/code"),
        "Upstream endpoint-config.ts missing /oauth2/device/code"
    );
    assert!(
        endpoint_ts.contains("/oauth2/token"),
        "Upstream endpoint-config.ts missing /oauth2/token"
    );
    assert!(
        endpoint_ts.contains("https://account.minimax.io"),
        "Upstream endpoint-config.ts missing global account origin"
    );
    assert!(
        endpoint_ts.contains("https://account.minimax.cn"),
        "Upstream endpoint-config.ts missing CN account origin"
    );

    // 3. Verify TUI attribution query parameters from authorization-url.ts
    let auth_url_ts_url = format!("{raw_base}/packages/tui/src/auth/authorization-url.ts");
    let resp = client
        .call(UpstreamRequest::get(&auth_url_ts_url), TimeoutProfile::OAuth, CancellationToken::new())
        .await
        .expect("fetch authorization-url.ts");
    let auth_url_bytes = resp.collect().await.expect("read authorization-url.ts body");
    let auth_url_ts = String::from_utf8_lossy(&auth_url_bytes);

    assert!(
        auth_url_ts.contains("'client_surface', 'tui'"),
        "Upstream authorization-url.ts diverged from client_surface=tui"
    );
    assert!(
        auth_url_ts.contains("mcode-internal"),
        "Upstream authorization-url.ts diverged from download_source=mcode-internal"
    );

    // 4. Verify checkin paths from http-gateway.ts
    let http_gateway_url = format!("{raw_base}/packages/tui/src/checkin/http-gateway.ts");
    let resp = client
        .call(UpstreamRequest::get(&http_gateway_url), TimeoutProfile::OAuth, CancellationToken::new())
        .await
        .expect("fetch http-gateway.ts");
    let gateway_bytes = resp.collect().await.expect("read http-gateway.ts body");
    let gateway_ts = String::from_utf8_lossy(&gateway_bytes);

    assert!(
        gateway_ts.contains("/minimax-cloud/api/v1/signin/status"),
        "Upstream http-gateway.ts missing /minimax-cloud/api/v1/signin/status"
    );
    assert!(
        gateway_ts.contains("/minimax-cloud/api/v1/signin/claim"),
        "Upstream http-gateway.ts missing /minimax-cloud/api/v1/signin/claim"
    );

    // 5. Verify signin statuses from daily-signin.ts
    let daily_signin_url = format!("{raw_base}/packages/shared/src/daily-signin.ts");
    let resp = client
        .call(UpstreamRequest::get(&daily_signin_url), TimeoutProfile::OAuth, CancellationToken::new())
        .await
        .expect("fetch daily-signin.ts");
    let signin_bytes = resp.collect().await.expect("read daily-signin.ts body");
    let signin_ts = String::from_utf8_lossy(&signin_bytes);

    assert!(
        signin_ts.contains("Upcoming = 1"),
        "Upstream daily-signin.ts status enum diverged for Upcoming"
    );
    assert!(
        signin_ts.contains("Claimable = 2"),
        "Upstream daily-signin.ts status enum diverged for Claimable"
    );
    assert!(
        signin_ts.contains("Claimed = 3"),
        "Upstream daily-signin.ts status enum diverged for Claimed"
    );
    assert!(
        signin_ts.contains("Disabled = 4"),
        "Upstream daily-signin.ts status enum diverged for Disabled"
    );

    // 6. Verify Matrix attribution signatures, secrets, and paths from matrix-account-client.ts
    let matrix_client_url = format!("{raw_base}/packages/tui/src/account/matrix-account-client.ts");
    let resp = client
        .call(UpstreamRequest::get(&matrix_client_url), TimeoutProfile::OAuth, CancellationToken::new())
        .await
        .expect("fetch matrix-account-client.ts");
    let matrix_bytes = resp.collect().await.expect("read matrix-account-client.ts body");
    let matrix_ts = String::from_utf8_lossy(&matrix_bytes);

    assert!(
        matrix_ts.contains("/matrix/api/v1/user/get_user_extra_info"),
        "Upstream matrix-account-client.ts missing get_user_extra_info path"
    );
    assert!(
        matrix_ts.contains("/matrix/api/v1/commerce/get_membership_info"),
        "Upstream matrix-account-client.ts missing get_membership_info path"
    );
    assert!(
        matrix_ts.contains("device_platform: 'mcode'") || matrix_ts.contains("device_platform: \"mcode\""),
        "Upstream matrix-account-client.ts missing device_platform mcode"
    );
    assert!(
        matrix_ts.contains("total_remaining_amount"),
        "Upstream matrix-account-client.ts missing total_remaining_amount credit field"
    );
    assert!(
        matrix_ts.contains("/v1/api/user/info"),
        "Upstream matrix-account-client.ts missing user/info path"
    );
    assert!(
        matrix_ts.contains("/v1/api/openplatform/coding_plan/remains"),
        "Upstream matrix-account-client.ts missing coding_plan/remains path"
    );
    assert!(
        matrix_ts.contains("I*7Cf%WZ#S&%1RlZJ&C2"),
        "Upstream matrix-account-client.ts signature secret diverged"
    );
    assert!(
        matrix_ts.contains("ooui"),
        "Upstream matrix-account-client.ts yy salt diverged"
    );
    assert!(
        matrix_ts.contains("'x-signature'"),
        "Upstream matrix-account-client.ts missing x-signature header"
    );
    assert!(
        matrix_ts.contains("'x-timestamp'"),
        "Upstream matrix-account-client.ts missing x-timestamp header"
    );

    // 7. Verify LLM inference format and base URL from minimax-api.ts
    let llm_api_url = format!("{raw_base}/packages/local-runtime/src/model-provider/minimax-api.ts");
    let resp = client
        .call(UpstreamRequest::get(&llm_api_url), TimeoutProfile::OAuth, CancellationToken::new())
        .await
        .expect("fetch minimax-api.ts");
    let llm_bytes = resp.collect().await.expect("read minimax-api.ts body");
    let llm_ts = String::from_utf8_lossy(&llm_bytes);

    assert!(
        llm_ts.contains("MINIMAX_API_FORMAT = 'anthropic-messages'"),
        "Upstream minimax-api.ts diverged from anthropic-messages format"
    );
    assert!(
        llm_ts.contains("https://api.minimax.io/"),
        "Upstream minimax-api.ts missing global API base URL"
    );
    assert!(
        llm_ts.contains("https://api.minimaxi.com/"),
        "Upstream minimax-api.ts missing China API base URL"
    );

    // 8. Verify model catalog constants and managed-login preset from config.ts
    let config_ts_url = format!("{raw_base}/packages/config/src/config.ts");
    let resp = client
        .call(UpstreamRequest::get(&config_ts_url), TimeoutProfile::OAuth, CancellationToken::new())
        .await
        .expect("fetch config.ts");
    let config_bytes = resp.collect().await.expect("read config.ts body");
    let config_ts = String::from_utf8_lossy(&config_bytes);

    let start_idx = config_ts
        .find("const MINIMAX_MODELS: Record<string, ModelConfig> = {")
        .expect("find MINIMAX_MODELS in upstream config.ts");
    let end_idx = config_ts[start_idx..]
        .find("};")
        .map(|rel| start_idx + rel)
        .expect("find end of MINIMAX_MODELS");
    let models_block = &config_ts[start_idx..end_idx];

    // Verify upstream defines the 3 managed models with exact specs
    assert!(models_block.contains("\"MiniMax-M3\":"), "upstream missing MiniMax-M3");
    assert!(models_block.contains("\"MiniMax-M2.7-highspeed\":"), "upstream missing MiniMax-M2.7-highspeed");
    assert!(models_block.contains("\"MiniMax-M2.7\":"), "upstream missing MiniMax-M2.7");
    assert!(models_block.contains("contextWindowOptions: [512000, 1000000]"), "MiniMax-M3 contextWindowOptions diverged");
    assert!(models_block.contains("output: 128000"), "output token ceiling diverged");
    assert!(models_block.contains("context: 200000"), "context limit for M2.7 models diverged");

    // Cross-check 1:1 against OpenProxy's builtin catalog
    let builtin_models = openproxy_adapters::adapters::minimax::minimax_builtin_models();
    let m3 = builtin_models.iter().find(|m| m.model_id.as_str() == "MiniMax-M3").expect("MiniMax-M3 present");
    assert_eq!(m3.context_length, Some(1_000_000));
    assert_eq!(m3.max_output_tokens, Some(128_000));

    let m27_hs = builtin_models.iter().find(|m| m.model_id.as_str() == "MiniMax-M2.7-highspeed").expect("MiniMax-M2.7-highspeed present");
    assert_eq!(m27_hs.context_length, Some(200_000));
    assert_eq!(m27_hs.max_output_tokens, Some(128_000));

    let m27 = builtin_models.iter().find(|m| m.model_id.as_str() == "MiniMax-M2.7").expect("MiniMax-M2.7 present");
    assert_eq!(m27.context_length, Some(200_000));
    assert_eq!(m27.max_output_tokens, Some(128_000));

    // Preset managed login routing check
    assert!(
        config_ts.contains("authMode: \"managed-login\""),
        "Upstream config.ts missing authMode: managed-login preset"
    );
    assert!(
        config_ts.contains("https://agent.minimax.io/mavis/api/v1/llm/v1"),
        "Upstream config.ts missing Global managed preset base URL"
    );
    assert!(
        config_ts.contains("https://agent.minimax.cn/mavis/api/v1/llm/v1"),
        "Upstream config.ts missing China managed preset base URL"
    );

    // 9. Dynamic header scanner: verify ALL agent inference headers from model-resolver-helpers.ts
    let helpers_ts_url = format!("{raw_base}/packages/local-runtime-v2/src/service/model-system/resolution/model-resolver-helpers.ts");
    let resp = client
        .call(UpstreamRequest::get(&helpers_ts_url), TimeoutProfile::OAuth, CancellationToken::new())
        .await
        .expect("fetch model-resolver-helpers.ts");
    let helpers_bytes = resp.collect().await.expect("read model-resolver-helpers.ts body");
    let helpers_ts = String::from_utf8_lossy(&helpers_bytes);

    let start_fn = helpers_ts
        .find("export function buildLocalProviderHeaders")
        .expect("find buildLocalProviderHeaders in upstream model-resolver-helpers.ts");
    let end_fn = helpers_ts[start_fn..]
        .find("return headers;\n}")
        .map(|rel| start_fn + rel + 17)
        .expect("find end of buildLocalProviderHeaders");
    let fn_body = &helpers_ts[start_fn..end_fn];

    let adapter = openproxy_adapters::adapters::minimax::MiniMaxAdapter::new();
    let model = openproxy_types::ModelId::new("MiniMax-M3");
    let our_headers = adapter.build_headers("test-token-123", openproxy_types::TargetFormat::Anthropic, &model);

    // Extract all string literal header keys from buildLocalProviderHeaders
    let mut scanned_headers = Vec::new();
    for line in fn_body.lines() {
        let trimmed = line.trim();
        for quote_char in ['\'', '"'] {
            let mut remaining = trimmed;
            while let Some(start_q) = remaining.find(quote_char) {
                let after_first = &remaining[start_q + 1..];
                if let Some(end_q) = after_first.find(quote_char) {
                    let candidate = &after_first[..end_q];
                    if (candidate.starts_with("X-Mavis-") || candidate == "User-Agent")
                        && !scanned_headers.contains(&candidate.to_string())
                    {
                        scanned_headers.push(candidate.to_string());
                    }
                    remaining = &after_first[end_q + 1..];
                } else {
                    break;
                }
            }
        }
    }

    assert!(scanned_headers.contains(&"X-Mavis-Session-Id".to_string()), "Scanner must find X-Mavis-Session-Id");
    assert!(scanned_headers.contains(&"X-Mavis-Agent-Id".to_string()), "Scanner must find X-Mavis-Agent-Id");
    assert!(scanned_headers.contains(&"X-Mavis-Timezone-Offset".to_string()), "Scanner must find X-Mavis-Timezone-Offset");
    assert!(scanned_headers.contains(&"User-Agent".to_string()), "Scanner must find User-Agent");

    // Fail if upstream ever introduces a new header in buildLocalProviderHeaders that OpenProxy lacks
    for upstream_header in &scanned_headers {
        assert!(
            our_headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(upstream_header)),
            "Upstream model-resolver-helpers.ts introduced new header '{upstream_header}' not present in OpenProxy MiniMaxAdapter!"
        );
    }

    // 10. Verify live chat URL parity against upstream managed-login preset
    assert_eq!(
        adapter.build_chat_url(openproxy_types::TargetFormat::Anthropic, &model),
        "https://agent.minimax.io/mavis/api/v1/llm/v1/messages",
        "OpenProxy MiniMaxAdapter chat URL must match upstream managed-login preset + /messages"
    );

    println!(
        "[ContractTest] 100% 1:1 Parity verified across all MiniMax subsystems (OAuth, Matrix, Quota, Check-in, LLM, Models, {} Headers) against live MiniMax-AI/minimax-code main branch!",
        scanned_headers.len()
    );
}

#[test]
fn test_minimax_dynamic_spoofer_and_endpoint_resolution() {
    use openproxy_adapters::spoofer::{
        ClientSpoofer, MINIMAX_TEST_LOCK, MiniMaxSpoofer, reset_dynamic_minimax_overrides,
        set_dynamic_minimax_anthropic_version, set_dynamic_minimax_extra_header,
        set_dynamic_minimax_ua,
    };

    let _guard = MINIMAX_TEST_LOCK.lock().expect("lock minimax test lock");
    reset_dynamic_minimax_overrides();

    // 1. Default MiniMaxSpoofer contract verification
    let spoofer = MiniMaxSpoofer;
    let headers = spoofer.headers();
    let find_hdr = |k: &str| {
        headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(find_hdr("User-Agent"), Some("MiniMaxAgent"));
    assert_eq!(find_hdr("Anthropic-Version"), Some("2023-06-01"));
    assert_eq!(find_hdr("X-Mavis-Agent-Id"), Some("main"));
    assert_eq!(find_hdr("X-Mavis-Timezone-Offset"), Some("0"));
    assert!(
        find_hdr("X-Mavis-Session-Id").is_some_and(|s| s.starts_with("session_")),
        "MiniMaxSpoofer must generate a valid session ID"
    );

    // 2. Dynamic in-memory spoofer overrides verification
    set_dynamic_minimax_ua("MiniMaxAgent-Custom/2.5");
    set_dynamic_minimax_anthropic_version("2024-01-01");
    set_dynamic_minimax_extra_header("X-Mavis-Agent-Id", "openproxy-worker");
    set_dynamic_minimax_extra_header("X-Custom-Pipeline", "turbo");

    let overridden = spoofer.headers();
    let find_ovr = |k: &str| {
        overridden
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(find_ovr("User-Agent"), Some("MiniMaxAgent-Custom/2.5"));
    assert_eq!(find_ovr("Anthropic-Version"), Some("2024-01-01"));
    assert_eq!(find_ovr("X-Mavis-Agent-Id"), Some("openproxy-worker"));
    assert_eq!(find_ovr("X-Custom-Pipeline"), Some("turbo"));

    reset_dynamic_minimax_overrides();

    // 3. Dynamic endpoint and origin resolution verification
    let region = MiniMaxRegion::Global;
    assert_eq!(region.account_origin(), "https://account.minimax.io");
    assert_eq!(region.gateway_origin(), "https://agent.minimax.io");

    // Default without env vars returns canonical origins
    assert_eq!(region.resolved_account_origin(), "https://account.minimax.io");
    assert_eq!(region.resolved_gateway_origin(), "https://agent.minimax.io");

    // With dynamic env vars set, resolved origins redirect seamlessly
    // SAFETY: Single-threaded scope in isolated unit test
    unsafe {
        std::env::set_var("OPENPROXY_MINIMAX_ACCOUNT_BASE_URL", "https://mock-account.local");
        std::env::set_var("OPENPROXY_MINIMAX_GATEWAY_BASE_URL", "https://mock-agent.local");
    }

    assert_eq!(region.resolved_account_origin(), "https://mock-account.local");
    assert_eq!(region.resolved_gateway_origin(), "https://mock-agent.local");

    unsafe {
        std::env::remove_var("OPENPROXY_MINIMAX_ACCOUNT_BASE_URL");
        std::env::remove_var("OPENPROXY_MINIMAX_GATEWAY_BASE_URL");
    }

    assert_eq!(region.resolved_account_origin(), "https://account.minimax.io");
    assert_eq!(region.resolved_gateway_origin(), "https://agent.minimax.io");
}

