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
