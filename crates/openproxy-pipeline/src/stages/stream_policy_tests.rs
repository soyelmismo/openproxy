use super::{FormattingStage, enforce_stream_body, resolve_upstream_streaming, stream_request_url};
use crate::context::{PipelineContext, ResolvedTarget};
use crate::test_utils::{make_request, test_config};
use openproxy_adapters::adapters::{CustomAdapter, ProviderAdapterConfig, ProviderAdapterEnum};
use openproxy_types::ids::{ComboId, ProviderId};
use openproxy_types::{AuthType, ProviderFormat, TargetFormat};
use std::sync::Arc;

#[test]
fn unary_is_a_veto_at_either_scope() {
    for model in [None, Some(false), Some(true)] {
        for provider in [None, Some(false), Some(true)] {
            assert_eq!(
                resolve_upstream_streaming(model, provider),
                model != Some(false) && provider != Some(false)
            );
        }
    }
}

#[test]
fn wrapper_forcing_or_stripping_cannot_override_policy() {
    for stream in [false, true] {
        for input in [
            r#"{"stream":true,"stream_options":{}}"#,
            r#"{"stream":false}"#,
            "{}",
        ] {
            let result = enforce_stream_body(
                bytes::Bytes::from_static(input.as_bytes()),
                TargetFormat::Openai,
                stream,
            )
            .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&result).unwrap();
            assert_eq!(body["stream"], stream);
            if !stream {
                assert!(body.get("stream_options").is_none());
            }
        }
        let result = enforce_stream_body(
            bytes::Bytes::from_static(br#"{"params":{"stream":true},"threadId":"test"}"#),
            TargetFormat::CommandCodeGo,
            stream,
        )
        .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(body["params"]["stream"], stream);
        assert!(body.get("stream").is_none());
    }
}

#[test]
fn gemini_and_antigravity_unary_select_non_sse_rpc() {
    for url in [
        "https://example.com/models/test:streamGenerateContent?alt=sse",
        "https://example.com/v1internal:streamGenerateContent?key=test&alt=sse",
    ] {
        assert_eq!(stream_request_url(url.into(), true), url);
        let unary = stream_request_url(url.into(), false);
        assert!(unary.contains(":generateContent"));
        assert!(!unary.contains("alt=sse"));
        if url.contains("key=test") {
            assert!(unary.ends_with("?key=test"));
        }
    }
}

#[tokio::test]
async fn formatting_stage_uses_policy_not_client_mode_or_sink() {
    let conn = Arc::new(parking_lot::Mutex::new(
        rusqlite::Connection::open_in_memory().unwrap(),
    ));
    for (format, target_format) in [
        (ProviderFormat::Openai, TargetFormat::Openai),
        (ProviderFormat::Anthropic, TargetFormat::Anthropic),
        (ProviderFormat::Responses, TargetFormat::Responses),
    ] {
        for model in [None, Some(false), Some(true)] {
            for provider in [None, Some(false), Some(true)] {
                let headers = provider
                    .map(|stream| {
                        vec![(
                            "X-OpenProxy-Stream-Mode".into(),
                            if stream { "stream" } else { "unary" }.into(),
                        )]
                    })
                    .unwrap_or_default();
                let adapter = ProviderAdapterEnum::Custom(Box::new(CustomAdapter::from_config(
                    ProviderAdapterConfig {
                        id: ProviderId::new("policy-test"),
                        name: "policy-test".into(),
                        base_url: "http://127.0.0.1:1".into(),
                        auth_type: AuthType::None,
                        format,
                        extra_headers: headers,
                        anonymous_fallback: false,
                        rate_limit_scope: "none".into(),
                    },
                )));
                let mut config =
                    test_config(Arc::new(openproxy_db::MasterKey::generate().unwrap()));
                config.adapters = Arc::new(vec![adapter]);
                let pipeline = crate::Pipeline::new(Arc::clone(&conn), config);
                for client_stream in [false, true] {
                    for with_sink in [false, true] {
                        let (mut req, _disconnect) = make_request(ComboId(1));
                        Arc::make_mut(&mut req.openai_request).stream = client_stream;
                        if !with_sink {
                            req.stream_sink = None;
                        }
                        let mut ctx = PipelineContext::new(req, pipeline.clone());
                        ctx.current_target = Some(ResolvedTarget {
                            target: openproxy_types::ComboTarget {
                                provider_id: ProviderId::new("policy-test"),
                                ..Default::default()
                            },
                            model: openproxy_types::models::Model {
                                model_id: "test-model".into(),
                                target_format,
                                capabilities_json: model.map(|stream| {
                                    serde_json::json!({"streaming":stream})
                                        .to_string()
                                        .into_boxed_str()
                                }),
                                ..Default::default()
                            },
                            api_key: String::new(),
                            api_key_label: None,
                            custom_meta: None,
                        });
                        // The end-of-chain error is intentional: stop after the REAL stage
                        // and inspect the context that DispatchStage would consume.
                        let chain = crate::pipeline_chain![FormattingStage];
                        let _ = chain.execute_nested(&mut ctx).await;
                        let expected = model != Some(false) && provider != Some(false);
                        assert_eq!(ctx.is_streaming, Some(expected));
                        let body: serde_json::Value = serde_json::from_slice(
                            ctx.body_bytes.as_ref().expect("formatted body"),
                        )
                        .unwrap();
                        assert_eq!(
                            body["stream"], expected,
                            "format={format:?} model={model:?} provider={provider:?} client={client_stream} sink={with_sink}"
                        );
                    }
                }
            }
        }
    }
}
