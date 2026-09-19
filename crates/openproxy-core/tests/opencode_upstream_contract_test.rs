//! Integration test for 1:1 contract parity between OpenProxy and upstream OpenCode Zen.
//!
//! Validates:
//! 1. Golden contract spec parity (100% offline, runs on every `cargo test`).
//! 2. Live remote upstream contract verification against `models.dev/api.json` and
//!    `github.com/anomalyco/opencode/dev` via lightweight remote HTTP queries without cloning.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use openproxy_adapters::ProviderAdapter;
use openproxy_adapters::adapters::opencode_common::{
    OpenCodeFlavor, OpenCodeZenAdapter, classify_opencode_target_format,
    inject_opencode_agent_quartet_tools, is_free_opencode_tier,
};
use openproxy_adapters::spoofer::{
    OPENCODE_UA, generate_request_id, generate_session_id, is_valid_opencode_request_id,
    is_valid_opencode_session_id,
};
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use openproxy_types::{ModelId, TargetFormat};

#[test]
fn test_opencode_zen_golden_contract_spec_parity() {
    let adapter = OpenCodeZenAdapter::new();

    // 1. Adapter identity, base URL, and anonymous fallback
    assert_eq!(adapter.id().as_str(), "opencode-zen");
    assert_eq!(adapter.config().base_url, "https://opencode.ai/zen/v1");
    assert!(
        adapter.is_anonymous_fallback(),
        "OpenCode Zen must support anonymous / free tier fallback"
    );

    // 2. Chat URL routing per format
    let model = ModelId::new("test-model");
    assert_eq!(
        adapter.build_chat_url(TargetFormat::Anthropic, &model),
        "https://opencode.ai/zen/v1/messages"
    );
    assert_eq!(
        adapter.build_chat_url(TargetFormat::Responses, &model),
        "https://opencode.ai/zen/v1/responses"
    );
    assert_eq!(
        adapter.build_chat_url(TargetFormat::Openai, &model),
        "https://opencode.ai/zen/v1/chat/completions"
    );
    assert_eq!(
        adapter.build_chat_url(TargetFormat::Gemini, &model),
        "https://opencode.ai/zen/v1/models/test-model:streamGenerateContent?alt=sse"
    );

    // 3. Auth and identity headers branching (PAID key)
    let headers_paid = adapter.build_headers("zen_sk_test123", TargetFormat::Anthropic, &model);
    fn find_hdr<'a>(hdrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
        hdrs.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    assert_eq!(find_hdr(&headers_paid, "x-api-key"), Some("zen_sk_test123"));
    assert_eq!(
        find_hdr(&headers_paid, "Anthropic-Version"),
        Some("2023-06-01")
    );
    assert_eq!(find_hdr(&headers_paid, "Authorization"), None);
    assert_eq!(find_hdr(&headers_paid, "User-Agent"), Some(OPENCODE_UA));
    assert_eq!(find_hdr(&headers_paid, "x-opencode-client"), Some("cli"));
    assert_eq!(
        find_hdr(&headers_paid, "x-opencode-project"),
        Some("global")
    );

    let session_id = find_hdr(&headers_paid, "x-opencode-session").unwrap();
    assert!(
        is_valid_opencode_session_id(session_id),
        "session id must be valid canonical format: {session_id}"
    );

    let request_id = find_hdr(&headers_paid, "x-opencode-request").unwrap();
    assert!(
        is_valid_opencode_request_id(request_id),
        "request id must be valid canonical format: {request_id}"
    );

    // 4. Auth headers branching (FREE / keyless tier)
    let headers_free_openai = adapter.build_headers("", TargetFormat::Openai, &model);
    assert_eq!(
        find_hdr(&headers_free_openai, "Authorization"),
        Some("Bearer public")
    );
    assert_eq!(find_hdr(&headers_free_openai, "x-api-key"), None);

    let headers_free_anthropic = adapter.build_headers("public", TargetFormat::Anthropic, &model);
    assert_eq!(
        find_hdr(&headers_free_anthropic, "x-api-key"),
        Some("public")
    );
    assert_eq!(
        find_hdr(&headers_free_anthropic, "Anthropic-Version"),
        Some("2023-06-01")
    );
    assert_eq!(find_hdr(&headers_free_anthropic, "Authorization"), None);

    let headers_free_gemini = adapter.build_headers("", TargetFormat::Gemini, &model);
    assert_eq!(
        find_hdr(&headers_free_gemini, "x-goog-api-key"),
        Some("public")
    );

    // 5. Free tier model detection
    assert!(is_free_opencode_tier(OpenCodeFlavor::Zen, "", &model));
    assert!(is_free_opencode_tier(OpenCodeFlavor::Zen, "public", &model));
    assert!(is_free_opencode_tier(
        OpenCodeFlavor::Zen,
        "paid-key",
        &ModelId::new("big-pickle")
    ));
    assert!(is_free_opencode_tier(
        OpenCodeFlavor::Zen,
        "paid-key",
        &ModelId::new("union-alpha")
    ));
    assert!(is_free_opencode_tier(
        OpenCodeFlavor::Zen,
        "paid-key",
        &ModelId::new("grok-code")
    ));
    assert!(is_free_opencode_tier(
        OpenCodeFlavor::Zen,
        "paid-key",
        &ModelId::new("mimo-v2.5-free")
    ));
    assert!(is_free_opencode_tier(
        OpenCodeFlavor::Zen,
        "paid-key",
        &ModelId::new("muse-spark-1.3-contributor-free")
    ));
    assert!(!is_free_opencode_tier(
        OpenCodeFlavor::Zen,
        "paid-key",
        &ModelId::new("claude-sonnet-4-6")
    ));

    // 6. models.dev routing format translation
    assert_eq!(
        openproxy_core::models_dev_sync::resolve_routing_format(Some("@ai-sdk/anthropic"), None),
        Some(TargetFormat::Anthropic)
    );
    assert_eq!(
        openproxy_core::models_dev_sync::resolve_routing_format(Some("@ai-sdk/google"), None),
        Some(TargetFormat::Gemini)
    );
    assert_eq!(
        openproxy_core::models_dev_sync::resolve_routing_format(Some("@ai-sdk/openai"), None),
        Some(TargetFormat::Responses)
    );
    assert_eq!(
        openproxy_core::models_dev_sync::resolve_routing_format(
            Some("@ai-sdk/openai-compatible"),
            None
        ),
        Some(TargetFormat::Openai)
    );
    assert_eq!(
        openproxy_core::models_dev_sync::resolve_routing_format(
            None,
            Some("@ai-sdk/openai-compatible")
        ),
        Some(TargetFormat::Openai)
    );

    // 7. Canonical jump-table classification fallback
    assert_eq!(
        classify_opencode_target_format(OpenCodeFlavor::Zen, "union-alpha"),
        TargetFormat::Anthropic
    );
    assert_eq!(
        classify_opencode_target_format(OpenCodeFlavor::Zen, "minimax-m3"),
        TargetFormat::Openai
    );
    assert_eq!(
        classify_opencode_target_format(OpenCodeFlavor::Zen, "grok-code"),
        TargetFormat::Openai
    );
    assert_eq!(
        classify_opencode_target_format(OpenCodeFlavor::Zen, "claude-sonnet-4-6"),
        TargetFormat::Anthropic
    );
    assert_eq!(
        classify_opencode_target_format(OpenCodeFlavor::Zen, "muse-spark-1.3"),
        TargetFormat::Responses
    );
    assert_eq!(
        classify_opencode_target_format(OpenCodeFlavor::Zen, "gemini-3.1-pro"),
        TargetFormat::Gemini
    );
}

#[test]
fn test_opencode_zen_agent_quartet_tools_injection() {
    // 1. OpenAI Chat Completions tool injection format
    let mut obj_openai = serde_json::Map::new();
    inject_opencode_agent_quartet_tools(&mut obj_openai, TargetFormat::Openai);
    let tools_oa = obj_openai["tools"].as_array().unwrap();
    assert_eq!(tools_oa.len(), 4);
    let names_oa: Vec<&str> = tools_oa
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names_oa, vec!["bash", "glob", "grep", "read"]);

    // 2. Anthropic Messages tool injection format
    let mut obj_anthropic = serde_json::Map::new();
    inject_opencode_agent_quartet_tools(&mut obj_anthropic, TargetFormat::Anthropic);
    let tools_anth = obj_anthropic["tools"].as_array().unwrap();
    assert_eq!(tools_anth.len(), 4);
    assert_eq!(tools_anth[0]["name"], "bash");
    assert!(tools_anth[0].get("input_schema").is_some());

    // 3. Responses API tool injection format
    let mut obj_responses = serde_json::Map::new();
    inject_opencode_agent_quartet_tools(&mut obj_responses, TargetFormat::Responses);
    let tools_resp = obj_responses["tools"].as_array().unwrap();
    assert_eq!(tools_resp.len(), 4);
    assert_eq!(tools_resp[0]["type"], "function");
    assert_eq!(tools_resp[0]["name"], "bash");
    assert!(tools_resp[0].get("parameters").is_some());

    // 4. Gemini streamGenerateContent tool injection format
    let mut obj_gemini = serde_json::Map::new();
    inject_opencode_agent_quartet_tools(&mut obj_gemini, TargetFormat::Gemini);
    let tools_gem = obj_gemini["tools"].as_array().unwrap();
    assert_eq!(tools_gem.len(), 1);
    let decls = tools_gem[0]["functionDeclarations"].as_array().unwrap();
    assert_eq!(decls.len(), 4);
    assert_eq!(decls[0]["name"], "bash");
}

#[test]
fn test_opencode_zen_request_wrapping_and_aliasing() {
    let adapter = OpenCodeZenAdapter::new();
    let target = openproxy_types::context::ResolvedTarget {
        target: openproxy_types::combos::ComboTarget {
            id: openproxy_types::ids::ComboTargetId(1),
            combo_id: openproxy_types::ids::ComboId(1),
            provider_id: openproxy_types::ids::ProviderId::new("opencode-zen"),
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
            thinking_effort: None,
        },
        model: openproxy_types::Model {
            row_id: openproxy_types::ModelRowId(1),
            provider_id: openproxy_types::ProviderId::new("opencode-zen"),
            model_id: "mimo-v2.5".into(),
            target_format: TargetFormat::Openai,
            active: true,
            ..Default::default()
        },
        api_key: String::new(), // Free tier
        api_key_label: None,
        custom_meta: None,
    };

    let body =
        bytes::Bytes::from(r#"{"model":"mimo-v2.5","messages":[{"role":"user","content":"hi"}]}"#);
    let wrapped = adapter
        .wrap_request_body(
            body,
            TargetFormat::Openai,
            &ModelId::new("mimo-v2.5"),
            &target,
        )
        .expect("wrap free tier request");

    let val: serde_json::Value = serde_json::from_slice(&wrapped).expect("valid json");
    assert_eq!(
        val["model"], "mimo-v2.5-free",
        "must auto-alias mimo-v2.5 to mimo-v2.5-free"
    );
    assert_eq!(
        val["stream"], true,
        "must enforce stream: true on free tier"
    );
    assert_eq!(
        val["tools"].as_array().unwrap().len(),
        4,
        "must inject quartet tools"
    );
}

#[tokio::test]
async fn test_opencode_zen_remote_upstream_live_contract_parity() {
    let client = UpstreamClient::new();
    let cancel = CancellationToken::new();

    // 1. Probe live models endpoint
    let probe_url = "https://opencode.ai/zen/v1/models";
    let mut req = UpstreamRequest::get(probe_url);
    req.headers.insert(
        axum::http::header::USER_AGENT,
        axum::http::HeaderValue::from_static(OPENCODE_UA),
    );
    req.headers.insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_static("Bearer public"),
    );

    let resp = match client
        .call(req, TimeoutProfile::OAuth, cancel.clone())
        .await
    {
        Ok(r) if r.status.is_success() => r,
        Ok(r) => {
            eprintln!(
                "[OpenCodeContractTest] Models probe HTTP {}, skipping live check",
                r.status
            );
            return;
        }
        Err(e) => {
            eprintln!("[OpenCodeContractTest] Network unreachable ({e}), skipping live check");
            return;
        }
    };

    let body_bytes = resp.collect().await.expect("read models body");
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).expect("parse models json");
    let models_data = json["data"].as_array().expect("data array");
    let ids: Vec<&str> = models_data
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();

    assert!(
        ids.contains(&"big-pickle"),
        "live catalogue must include big-pickle"
    );
    assert!(
        ids.contains(&"claude-sonnet-4-6"),
        "live catalogue must include claude-sonnet-4-6"
    );

    // 2. Probe live free tier chat completions endpoint with big-pickle
    let chat_url = "https://opencode.ai/zen/v1/chat/completions";
    let chat_body = serde_json::json!({
        "model": "big-pickle",
        "stream": true,
        "messages": [{"role": "user", "content": "ping"}],
        "tools": [
            {"type": "function", "function": {"name": "bash", "description": "cmd", "parameters": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}}},
            {"type": "function", "function": {"name": "glob", "description": "glob", "parameters": {"type": "object", "properties": {"pattern": {"type": "string"}}, "required": ["pattern"]}}},
            {"type": "function", "function": {"name": "grep", "description": "grep", "parameters": {"type": "object", "properties": {"pattern": {"type": "string"}}, "required": ["pattern"]}}},
            {"type": "function", "function": {"name": "read", "description": "read", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}}
        ]
    });

    let mut chat_req = UpstreamRequest::post_json(
        chat_url,
        bytes::Bytes::from(serde_json::to_vec(&chat_body).unwrap()),
    );
    chat_req.headers.insert(
        axum::http::header::USER_AGENT,
        axum::http::HeaderValue::from_static(OPENCODE_UA),
    );
    chat_req.headers.insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_static("Bearer public"),
    );
    chat_req.headers.insert(
        axum::http::HeaderName::from_static("x-opencode-client"),
        axum::http::HeaderValue::from_static("cli"),
    );
    chat_req.headers.insert(
        axum::http::HeaderName::from_static("x-opencode-project"),
        axum::http::HeaderValue::from_static("global"),
    );
    chat_req.headers.insert(
        axum::http::HeaderName::from_static("x-opencode-session"),
        axum::http::HeaderValue::from_str(&generate_session_id()).unwrap(),
    );
    chat_req.headers.insert(
        axum::http::HeaderName::from_static("x-opencode-request"),
        axum::http::HeaderValue::from_str(&generate_request_id()).unwrap(),
    );

    let chat_resp = match client.call(chat_req, TimeoutProfile::OAuth, cancel).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[OpenCodeContractTest] Live chat inference failed ({e}), skipping");
            return;
        }
    };

    assert_eq!(
        chat_resp.status.as_u16(),
        200,
        "live big-pickle free tier inference must return 200 OK"
    );
}

#[tokio::test]
async fn test_opencode_upstream_repo_headers_drift_detection() {
    let client = UpstreamClient::new();
    let cancel = CancellationToken::new();

    let known_client_headers = [
        "x-opencode-client",
        "x-opencode-project",
        "x-opencode-session",
        "x-opencode-request",
    ];

    let known_server_headers = [
        "x-opencode-client",
        "x-opencode-project",
        "x-opencode-session",
        "x-opencode-request",
        "x-opencode-endpoint-id",
        "x-opencode-upstream-model-id",
    ];

    // 1. Probe upstream client request.ts for new x-opencode headers
    let client_url = "https://raw.githubusercontent.com/anomalyco/opencode/dev/packages/opencode/src/session/llm/request.ts";
    let req = UpstreamRequest::get(client_url);
    if let Ok(resp) = client
        .call(req, TimeoutProfile::OAuth, cancel.clone())
        .await
        && resp.status.is_success()
        && let Ok(body) = resp.collect().await
    {
        let text = String::from_utf8_lossy(&body);
        let re = regex::Regex::new(r#""(x-opencode-[a-z0-9-]+)""#).expect("regex");
        for cap in re.captures_iter(&text) {
            let header = &cap[1];
            assert!(
                known_client_headers.contains(&header),
                "Upstream OpenCode added new client header '{header}' in request.ts! Update OpenProxy to support it."
            );
        }
    }

    // 2. Probe upstream console zen handler.ts for new x-opencode headers
    let handler_url = "https://raw.githubusercontent.com/anomalyco/opencode/dev/packages/console/app/src/routes/zen/util/handler.ts";
    let handler_req = UpstreamRequest::get(handler_url);
    if let Ok(resp) = client
        .call(handler_req, TimeoutProfile::OAuth, cancel)
        .await
        && resp.status.is_success()
        && let Ok(body) = resp.collect().await
    {
        let text = String::from_utf8_lossy(&body);
        let re = regex::Regex::new(r#""(x-opencode-[a-z0-9-]+)""#).expect("regex");
        for cap in re.captures_iter(&text) {
            let header = &cap[1];
            assert!(
                known_server_headers.contains(&header),
                "Upstream OpenCode Zen handler references new header '{header}'! Update OpenProxy to support it."
            );
        }
    }
}
