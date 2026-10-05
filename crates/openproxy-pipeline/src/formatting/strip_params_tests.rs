use super::*;
use openproxy_adapters::adapters::custom_adapter::CustomAdapter;
use openproxy_adapters::adapters::{AdapterAuthType, ProviderAdapterConfig, ProviderAdapterEnum};
use openproxy_types::models::Model;
use openproxy_types::{
    ModelId, ModelRowId, OpenAIMessage, OpenAIRequest, ProviderFormat, ProviderId, TargetFormat,
};
use serde_json::{Value, json};
use std::sync::Arc;

fn test_req(openai_req: OpenAIRequest) -> crate::PipelineRequest {
    crate::PipelineRequest {
        request_id: openproxy_types::RequestId::new(),
        trace_id: openproxy_types::TraceId::new(),
        combo_id: openproxy_types::ComboId(1),
        openai_request: Arc::new(openai_req),
        client_disconnected: tokio::sync::watch::channel(None).1,
        stream_sink: None,
        api_key_id: None,
        combo_override: None,
        targets_override: None,
        request_headers: Default::default(),
        request_body_json: None,
        race_cancelled: false,
        race_cancel: None,
        endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
        compressed_messages: Arc::new(std::sync::OnceLock::new()),
        pii_session: Arc::new(parking_lot::Mutex::new(None)),
        compression_stats: Arc::new(parking_lot::Mutex::new(None)),
        proxy_override: None,
    }
}

fn test_model(provider_id: &str) -> Model {
    Model {
        row_id: ModelRowId(1),
        provider_id: ProviderId::new(provider_id),
        model_id: ModelId::new("zai-org/GLM-5.3-Flash"),
        display_name: Some("GLM 5.3 Flash".into()),
        target_format: TargetFormat::Openai,
        discovered_at: "2026-01-01T00:00:00Z".into(),
        context_length: Some(128000),
        active: true,
        custom: true,
        ..Default::default()
    }
}

fn make_custom_adapter(extra_headers: Vec<(String, String)>) -> ProviderAdapterEnum {
    ProviderAdapterEnum::Custom(Box::new(CustomAdapter::from_config(
        ProviderAdapterConfig {
            id: ProviderId::new("dahl"),
            name: "Dahl Custom".into(),
            base_url: "http://144.124.251.24:10800".into(),
            auth_type: AdapterAuthType::Bearer,
            format: ProviderFormat::Openai,
            extra_headers,
            anonymous_fallback: false,
            rate_limit_scope: "account".into(),
        },
    )))
}

#[test]
fn test_strip_provider_configured_params_helper() {
    let mut map = serde_json::Map::new();
    map.insert("session_id".into(), json!("sess-123"));
    map.insert("conversation_id".into(), json!("conv-456"));
    map.insert("keep_me".into(), json!("val"));
    let mut cow = std::borrow::Cow::Borrowed(&map);

    let headers = vec![(
        "X-OpenProxy-Strip-Params".to_string(),
        "session_id, conversation_id, non_existent".to_string(),
    )];

    strip_provider_configured_params(&mut cow, &headers);
    assert!(!cow.contains_key("session_id"));
    assert!(!cow.contains_key("conversation_id"));
    assert_eq!(cow.get("keep_me"), Some(&json!("val")));
}

#[test]
fn test_openai_formatter_strips_session_id_when_configured() {
    let adapter = make_custom_adapter(vec![(
        "X-OpenProxy-Strip-Params".into(),
        "session_id".into(),
    )]);

    let mut extra = serde_json::Map::new();
    extra.insert("session_id".into(), json!("sess-abc"));
    extra.insert("preserve_field".into(), json!("keep"));

    let openai_req = OpenAIRequest {
        model: "zai-org/GLM-5.3-Flash".into(),
        messages: vec![OpenAIMessage {
            role: "user".into(),
            content: Some(json!("hello")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        }],
        extra,
        ..Default::default()
    };

    let req = test_req(openai_req.clone());
    let formatted = OpenaiFormatter
        .format_request(
            &req,
            &test_model("dahl"),
            &openai_req.messages,
            true,
            &adapter,
        )
        .expect("formatting should succeed");

    let val: Value = serde_json::from_slice(&formatted).unwrap();
    assert!(
        val.get("session_id").is_none(),
        "session_id must be stripped for dahl"
    );
    assert_eq!(
        val.get("preserve_field"),
        Some(&json!("keep")),
        "other fields preserved"
    );
}

#[test]
fn test_openai_formatter_preserves_session_id_when_not_configured() {
    // Providers that require session_id in the body do NOT set X-OpenProxy-Strip-Params
    let adapter = make_custom_adapter(vec![("User-Agent".into(), "CustomClient/1.0".into())]);

    let mut extra = serde_json::Map::new();
    extra.insert("session_id".into(), json!("sess-required-by-provider"));
    extra.insert("preserve_field".into(), json!("keep"));

    let openai_req = OpenAIRequest {
        model: "zai-org/GLM-5.3-Flash".into(),
        messages: vec![OpenAIMessage {
            role: "user".into(),
            content: Some(json!("hello")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        }],
        extra,
        ..Default::default()
    };

    let req = test_req(openai_req.clone());
    let formatted = OpenaiFormatter
        .format_request(
            &req,
            &test_model("dahl"),
            &openai_req.messages,
            true,
            &adapter,
        )
        .expect("formatting should succeed");

    let val: Value = serde_json::from_slice(&formatted).unwrap();
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
