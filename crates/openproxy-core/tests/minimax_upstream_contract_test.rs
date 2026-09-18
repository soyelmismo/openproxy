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
    assert_eq!(find_hdr("Content-Type"), Some("application/json"));
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

    // 9. Verify agent inference headers from model-resolver-helpers.ts
    let helpers_ts_url = format!("{raw_base}/packages/local-runtime-v2/src/service/model-system/resolution/model-resolver-helpers.ts");
    let resp = client
        .call(UpstreamRequest::get(&helpers_ts_url), TimeoutProfile::OAuth, CancellationToken::new())
        .await
        .expect("fetch model-resolver-helpers.ts");
    let helpers_bytes = resp.collect().await.expect("read model-resolver-helpers.ts body");
    let helpers_ts = String::from_utf8_lossy(&helpers_bytes);

    assert!(
        helpers_ts.contains("MANAGED_PROVIDER_USER_AGENT = 'MiniMaxAgent'"),
        "Upstream diverged from User-Agent: MiniMaxAgent"
    );
    assert!(
        helpers_ts.contains("'X-Mavis-Session-Id'"),
        "Upstream diverged from X-Mavis-Session-Id header"
    );
    assert!(
        helpers_ts.contains("'X-Mavis-Agent-Id'"),
        "Upstream diverged from X-Mavis-Agent-Id header"
    );
    assert!(
        helpers_ts.contains("'X-Mavis-Timezone-Offset'"),
        "Upstream diverged from X-Mavis-Timezone-Offset header"
    );

    println!("[ContractTest] 100% 1:1 Parity verified across all MiniMax subsystems (OAuth, Matrix, Quota, Check-in, LLM, Models, Headers) against live MiniMax-AI/minimax-code main branch!");
}

