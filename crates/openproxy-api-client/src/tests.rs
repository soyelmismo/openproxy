use super::*;
use serde_json::json;

#[test]
fn trims_trailing_slash() {
    let c = Client::new("http://example.com/");
    assert_eq!(c.url("/admin/health"), "http://example.com/admin/health");
}

#[test]
fn urlencoded_keeps_unreserved() {
    assert_eq!(urlencoded("openrouter"), "openrouter");
    assert_eq!(urlencoded("openai/gpt-4o"), "openai/gpt-4o");
    assert_eq!(urlencoded("2026-01-15T00:00:00Z"), "2026-01-15T00:00:00Z");
}

#[test]
fn urlencoded_escapes_spaces_and_amp() {
    assert_eq!(urlencoded("a b"), "a%20b");
    assert_eq!(urlencoded("a&b"), "a%26b");
}

#[test]
fn build_query_skips_nones() {
    let q = build_query(&[
        ("from", Some("2026-01-01T00:00:00Z")),
        ("to", None),
        ("provider_id", Some("openrouter")),
    ]);
    assert_eq!(q, "from=2026-01-01T00:00:00Z&provider_id=openrouter");
}

#[test]
fn usage_filter_query_serializes_known_fields() {
    let f = UsageFilter {
        from: Some("2026-01-01T00:00:00Z".to_string()),
        to: None,
        provider_id: Some(ProviderId::new("openrouter")),
        model_id: Some("openai/gpt-4o".to_string()),
        account_id: Some(AccountId::new(7)),
        combo_id: None,
        api_key_id: None,
    };
    let q = usage_filter_query(&f);
    assert!(q.contains("from=2026-01-01T00:00:00Z"));
    assert!(q.contains("provider_id=openrouter"));
    assert!(q.contains("model_id=openai/gpt-4o"));
    assert!(q.contains("account_id=7"));
    assert!(!q.contains("combo_id="));
    assert!(!q.contains("to="));
}

#[test]
fn map_error_body_recognizes_known_codes() {
    let body = serde_json::json!({
        "error": { "code": "validation", "message": "bad input" }
    })
    .to_string();
    let bytes = body.as_bytes();
    let err = map_error_body(400, bytes);
    match err {
        ClientError::Api(CoreError::Validation(msg)) => assert_eq!(msg, "bad input"),
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[test]
fn map_error_body_unknown_code_falls_back_to_status() {
    let body = serde_json::json!({
        "error": { "code": "made_up_code", "message": "wat" }
    })
    .to_string();
    let err = map_error_body(500, body.as_bytes());
    match err {
        ClientError::Status(500, msg) => assert!(msg.contains("made_up_code")),
        other => panic!("expected Status, got {other:?}"),
    }
}

#[test]
fn map_error_body_non_json_falls_back_to_status() {
    let err = map_error_body(502, b"<html>oops</html>");
    match err {
        ClientError::Status(502, msg) => assert!(msg.contains("oops")),
        other => panic!("expected Status, got {other:?}"),
    }
}

#[test]
fn test_list_providers() {
    unsafe {
        std::env::set_var("OPENPROXY_ALLOW_PRIVATE_UPSTREAMS", "true");
    }
    let server = httpmock::MockServer::start();
    let client = Client::new(server.base_url());

    let providers = vec![json!({
        "id": "p1",
        "name": "Provider 1",
        "base_url": "http://p1",
        "auth_type": "bearer",
        "format": "openai",
        "active": true,
        "created_at": "2026-01-01T00:00:00Z",
        "rate_limit_scope": "account"
    })];

    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/admin/providers");
        then.status(200).json_body(json!(providers));
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    let res = rt.block_on(client.list_providers()).unwrap();
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].id.as_str(), "p1");
}

#[test]
fn test_refresh_models() {
    unsafe {
        std::env::set_var("OPENPROXY_ALLOW_PRIVATE_UPSTREAMS", "true");
    }
    let server = httpmock::MockServer::start();
    let client = Client::new(server.base_url());

    server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/admin/models/1/refresh");
        then.status(200).json_body(json!({ "touched": 5 }));
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    let touched = rt.block_on(client.refresh_models(ModelRowId(1))).unwrap();
    assert_eq!(touched, 5);
}

#[test]
fn test_list_combo_targets() {
    unsafe {
        std::env::set_var("OPENPROXY_ALLOW_PRIVATE_UPSTREAMS", "true");
    }
    let server = httpmock::MockServer::start();
    let client = Client::new(server.base_url());

    let targets = vec![json!({
        "id": 1,
        "combo_id": 10,
        "provider_id": "p1",
        "account_id": 100,
        "model_row_id": 1000,
        "priority_order": 1,
        "weight": 100,
        "active": true
    })];

    server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/admin/combos/10/targets");
        then.status(200).json_body(json!(targets));
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    let res = rt.block_on(client.list_combo_targets(ComboId(10))).unwrap();
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].provider_id.as_str(), "p1");
}

fn check_string_errors() {
    assert!(matches!(
        CoreError::from_code_and_message("auth", "unauthorized"),
        Some(CoreError::Auth(msg)) if msg == "unauthorized"
    ));
    assert!(matches!(
        CoreError::from_code_and_message("validation", "invalid param"),
        Some(CoreError::Validation(msg)) if msg == "invalid param"
    ));
    assert!(matches!(
        CoreError::from_code_and_message("provider_not_found", "missing provider"),
        Some(CoreError::ProviderNotFound(msg)) if msg == "missing provider"
    ));
    assert!(matches!(
        CoreError::from_code_and_message("upstream_connection", "conn reset"),
        Some(CoreError::UpstreamConnection(msg)) if msg == "conn reset"
    ));
    assert!(matches!(
        CoreError::from_code_and_message("parse_error", "bad json"),
        Some(CoreError::Parse(msg)) if msg == "bad json"
    ));
    assert!(matches!(
        CoreError::from_code_and_message("config", "bad cfg"),
        Some(CoreError::Config(msg)) if msg == "bad cfg"
    ));
    assert!(matches!(
        CoreError::from_code_and_message("database", "sqlite lock"),
        Some(CoreError::Database { message, .. }) if message == "sqlite lock"
    ));
    assert!(matches!(
        CoreError::from_code_and_message("migration", "mismatch"),
        Some(CoreError::Migration { message, .. }) if message == "mismatch"
    ));
    assert!(matches!(
        CoreError::from_code_and_message("internal", "panic"),
        Some(CoreError::Internal(msg)) if msg == "panic"
    ));
}

fn check_id_errors() {
    assert!(matches!(
        CoreError::from_code_and_message("account_not_found", "42"),
        Some(CoreError::AccountNotFound(42))
    ));
    assert!(CoreError::from_code_and_message("account_not_found", "invalid").is_none());
    assert!(matches!(
        CoreError::from_code_and_message("combo_not_found", "100"),
        Some(CoreError::ComboNotFound(100))
    ));
    assert!(CoreError::from_code_and_message("combo_not_found", "not_an_id").is_none());
    assert!(matches!(
        CoreError::from_code_and_message("no_healthy_targets", "5"),
        Some(CoreError::NoHealthyTargets(5))
    ));
    assert!(CoreError::from_code_and_message("no_healthy_targets", "nan").is_none());
}

fn check_custom_errors() {
    use openproxy_types::CancelReason;

    assert!(matches!(
        CoreError::from_code_and_message("model_not_found", "gpt-4"),
        Some(CoreError::ModelNotFound { model, .. }) if model == "gpt-4"
    ));
    assert!(matches!(
        CoreError::from_code_and_message("upstream_timeout", "timeout msg"),
        Some(CoreError::UpstreamTimeout { .. })
    ));
    assert!(matches!(
        CoreError::from_code_and_message("upstream_error", "server 500"),
        Some(CoreError::UpstreamError { body, .. }) if body == "server 500"
    ));
    assert!(matches!(
        CoreError::from_code_and_message("rate_limited", "slow down"),
        Some(CoreError::RateLimited { .. })
    ));
    assert!(matches!(
        CoreError::from_code_and_message("client_disconnected", "drop"),
        Some(CoreError::Cancelled(CancelReason::ClientDisconnected))
    ));
    assert!(matches!(
        CoreError::from_code_and_message("race_lost", "lost"),
        Some(CoreError::RaceLost)
    ));
    assert!(CoreError::from_code_and_message("unrecognized_code", "some error").is_none());
}

#[test]
fn test_core_error_from_code() {
    check_string_errors();
    check_id_errors();
    check_custom_errors();
}
