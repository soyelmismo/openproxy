use super::*;
use serde_json::{Value, json};

#[test]
fn test_strip_provider_configured_params_from_body_removes_multiple_params() {
    let body = bytes::Bytes::from(
        serde_json::to_vec(&json!({
            "session_id": "sess-123",
            "conversation_id": "conv-456",
            "keep_me": "val"
        }))
        .unwrap(),
    );
    let extra_headers = vec![(
        "X-OpenProxy-Strip-Params".to_string(),
        "session_id, conversation_id, non_existent".to_string(),
    )];

    let stripped = strip_provider_configured_params_from_body(body, &extra_headers)
        .expect("strip should succeed");
    let val: Value = serde_json::from_slice(&stripped).unwrap();

    assert!(val.get("session_id").is_none());
    assert!(val.get("conversation_id").is_none());
    assert_eq!(val.get("keep_me"), Some(&json!("val")));
}

#[test]
fn test_strip_provider_configured_params_from_body_removes_session_id() {
    let extra_headers = vec![(
        "X-OpenProxy-Strip-Params".to_string(),
        "session_id".to_string(),
    )];

    // A fully-formatted upstream body, as produced for any target dialect.
    let body = bytes::Bytes::from(
        serde_json::to_vec(&json!({
            "session_id": "sess-abc",
            "preserve_field": "keep",
            "model": "zai-org/GLM-5.3-Flash"
        }))
        .unwrap(),
    );

    let stripped = strip_provider_configured_params_from_body(body, &extra_headers)
        .expect("strip should succeed");
    let val: Value = serde_json::from_slice(&stripped).unwrap();

    assert!(
        val.get("session_id").is_none(),
        "session_id must be stripped when configured"
    );
    assert_eq!(
        val.get("preserve_field"),
        Some(&json!("keep")),
        "other fields preserved"
    );
}

#[test]
fn test_strip_provider_configured_params_from_body_noop_when_not_configured() {
    // Providers that require session_id in the body do NOT set X-OpenProxy-Strip-Params
    let extra_headers = vec![("User-Agent".to_string(), "CustomClient/1.0".to_string())];

    let body = bytes::Bytes::from(
        serde_json::to_vec(&json!({
            "session_id": "sess-required-by-provider",
            "preserve_field": "keep"
        }))
        .unwrap(),
    );

    let stripped = strip_provider_configured_params_from_body(body, &extra_headers)
        .expect("strip should succeed");
    let val: Value = serde_json::from_slice(&stripped).unwrap();

    assert_eq!(
        val.get("session_id"),
        Some(&json!("sess-required-by-provider")),
        "session_id must be preserved when strip directive is absent"
    );
}

#[test]
fn test_populate_upstream_headers_never_leaks_openproxy_internal_headers() {
    let mut upstream_req = openproxy_adapters::upstream::UpstreamRequest::post_json(
        "http://example.com/v1/chat/completions".to_string(),
        bytes::Bytes::new(),
    );
    let headers = vec![
        (
            "X-OpenProxy-Strip-Params".to_string(),
            "session_id".to_string(),
        ),
        ("x-openproxy-custom".to_string(), "hidden".to_string()),
        ("User-Agent".to_string(), "OpenProxy/1.0".to_string()),
        ("X-Custom-Public".to_string(), "hello".to_string()),
    ];

    crate::dispatcher::unary::populate_upstream_headers(&mut upstream_req, &headers);

    assert!(
        upstream_req
            .headers
            .get("x-openproxy-strip-params")
            .is_none()
    );
    assert!(upstream_req.headers.get("x-openproxy-custom").is_none());
    assert_eq!(
        upstream_req
            .headers
            .get("user-agent")
            .unwrap()
            .to_str()
            .unwrap(),
        "OpenProxy/1.0"
    );
    assert_eq!(
        upstream_req
            .headers
            .get("x-custom-public")
            .unwrap()
            .to_str()
            .unwrap(),
        "hello"
    );
}

#[test]
fn test_populate_upstream_headers_marks_authorization_sensitive() {
    let mut upstream_req = openproxy_adapters::upstream::UpstreamRequest::post_json(
        "http://example.com/v1/chat/completions".to_string(),
        bytes::Bytes::new(),
    );
    let headers = vec![
        (
            "Authorization".to_string(),
            "Bearer secret-token-value".to_string(),
        ),
        ("X-Non-Sensitive".to_string(), "public-info".to_string()),
    ];

    crate::dispatcher::unary::populate_upstream_headers(&mut upstream_req, &headers);

    let auth_val = upstream_req
        .headers
        .get(http::header::AUTHORIZATION)
        .expect("authorization header present");
    assert!(
        auth_val.is_sensitive(),
        "authorization must be marked sensitive"
    );
    assert!(
        !format!("{auth_val:?}").contains("secret-token-value"),
        "authorization value must not appear in debug representation"
    );

    let public_val = upstream_req
        .headers
        .get("x-non-sensitive")
        .expect("public header present");
    assert!(
        !public_val.is_sensitive(),
        "public header must not be sensitive"
    );
}

#[test]
fn test_ensure_streaming_accept_sets_sse_only_for_streaming() {
    use openproxy_adapters::upstream::UpstreamRequest;

    // Streaming sin Accept previo -> se anuncia text/event-stream.
    let mut streaming = UpstreamRequest::post_json(
        "http://example.com/v1/messages".to_string(),
        bytes::Bytes::new(),
    );
    streaming.is_streaming = true;
    crate::dispatcher::unary::ensure_streaming_accept(&mut streaming);
    assert_eq!(
        streaming
            .headers
            .get(http::header::ACCEPT)
            .unwrap()
            .to_str()
            .unwrap(),
        "text/event-stream"
    );

    // No-streaming -> no se toca Accept.
    let mut unary = UpstreamRequest::post_json(
        "http://example.com/v1/messages".to_string(),
        bytes::Bytes::new(),
    );
    unary.is_streaming = false;
    crate::dispatcher::unary::ensure_streaming_accept(&mut unary);
    assert!(unary.headers.get(http::header::ACCEPT).is_none());

    // Streaming con Accept ya declarado por el adapter -> se respeta.
    let mut preset = UpstreamRequest::post_json(
        "http://example.com/v1/messages".to_string(),
        bytes::Bytes::new(),
    );
    preset.is_streaming = true;
    preset.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );
    crate::dispatcher::unary::ensure_streaming_accept(&mut preset);
    assert_eq!(
        preset
            .headers
            .get(http::header::ACCEPT)
            .unwrap()
            .to_str()
            .unwrap(),
        "application/json"
    );
}

#[test]
fn test_nvidia_nim_adapter_appends_strip_headers_and_strips_session_id() {
    use openproxy_adapters::adapters::ProviderAdapterEnum;

    let mut adapter =
        ProviderAdapterEnum::from_provider_id("nvidia-nim").expect("nvidia-nim adapter must exist");

    // Simulate rebuild_adapters applying extra_headers from db
    adapter.append_extra_headers(vec![(
        "X-OpenProxy-Strip-Params".to_string(),
        "session_id".to_string(),
    )]);

    // Ensure extra_headers was successfully appended via config_mut
    assert!(
        adapter
            .config()
            .extra_headers
            .iter()
            .any(|(k, v)| k == "X-OpenProxy-Strip-Params" && v == "session_id"),
        "extra_headers must contain X-OpenProxy-Strip-Params"
    );

    let body = bytes::Bytes::from(
        serde_json::to_vec(&json!({
            "model": "meta/llama-3.1-70b-instruct",
            "session_id": "sess-nvidia-12345",
            "messages": [{"role": "user", "content": "hi"}],
            "temperature": 0.7
        }))
        .unwrap(),
    );

    let stripped =
        strip_provider_configured_params_from_body(body, &adapter.config().extra_headers)
            .expect("stripping must succeed");
    let val: Value = serde_json::from_slice(&stripped).unwrap();

    assert!(
        val.get("session_id").is_none(),
        "session_id must be stripped from nvidia-nim payload"
    );
    assert_eq!(
        val.get("model"),
        Some(&json!("meta/llama-3.1-70b-instruct")),
        "model must be preserved"
    );
}
