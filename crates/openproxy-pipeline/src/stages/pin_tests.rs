//! Offline tests for the request-local adapter pin.
//!
//! The pipeline must resolve a provider adapter exactly once per request and
//! reuse that exact instance for the body wrap, the headers, the chat URL and
//! the finalization — even when `PipelineConfig::adapters` is refreshed in
//! between (OAuth refresh, hot config reload, discovery) and even when the
//! request is cloned by a retry or a race.
//!
//! No network: the adapters only serve configuration, and the only stage
//! executed is the real `FormattingStage`, which stops at the end of its
//! chain so the context can be inspected.

use super::FormattingStage;
use crate::context::{PipelineContext, ResolvedTarget, pinned_adapter};
use crate::test_utils::{make_request, test_config};
use openproxy_adapters::adapters::{CustomAdapter, ProviderAdapterConfig, ProviderAdapterEnum};
use openproxy_types::ids::{ComboId, ProviderId};
use openproxy_types::models::Model;
use openproxy_types::{AuthType, ComboTarget, ProviderFormat, TargetFormat};
use std::sync::Arc;

const PROVIDER_ID: &str = "pin-test";
/// Generation 1 of the adapter: unary upstream.
const GEN1_PORT: &str = "1";
/// Generation 2, as installed by a refresh between the wrap and the dispatch.
const GEN2_PORT: &str = "2";

/// A real `CustomAdapter` whose "generation" is observable through every
/// consumer: the streaming policy it forces (`x-openproxy-stream`) and the
/// base URL the chat URL is built from.
fn adapter_generation(unary: bool) -> ProviderAdapterEnum {
    ProviderAdapterEnum::Custom(Box::new(CustomAdapter::from_config(
        ProviderAdapterConfig {
            id: ProviderId::new(PROVIDER_ID),
            name: "generation-test".into(),
            base_url: format!(
                "http://127.0.0.1:{}",
                if unary { GEN1_PORT } else { GEN2_PORT }
            ),
            auth_type: AuthType::None,
            format: ProviderFormat::Openai,
            extra_headers: vec![(
                "x-openproxy-stream".into(),
                if unary { "false" } else { "true" }.into(),
            )],
            anonymous_fallback: false,
            rate_limit_scope: "none".into(),
        },
    )))
}

fn resolved_target(target_format: TargetFormat) -> ResolvedTarget {
    ResolvedTarget {
        target: ComboTarget {
            provider_id: ProviderId::new(PROVIDER_ID),
            ..Default::default()
        },
        model: Model {
            model_id: "m".into(),
            target_format,
            ..Default::default()
        },
        api_key: String::new(),
        api_key_label: None,
        custom_meta: None,
    }
}

/// Pipeline whose registry holds generation `unary`, plus the provider id.
fn harness(unary: bool) -> (crate::Pipeline, ProviderId) {
    let conn = test_connection();
    let master = Arc::new(openproxy_db::MasterKey::generate().unwrap());
    let mut config = test_config(master);
    config.adapters = Arc::new(vec![adapter_generation(unary)]);
    (
        crate::Pipeline::new(conn, config),
        ProviderId::new(PROVIDER_ID),
    )
}

/// Offline in-memory connection: the pin never touches the database.
fn test_connection() -> Arc<parking_lot::Mutex<rusqlite::Connection>> {
    Arc::new(parking_lot::Mutex::new(
        rusqlite::Connection::open_in_memory().unwrap(),
    ))
}

#[tokio::test]
async fn a_refresh_between_wrap_and_dispatch_does_not_change_the_pin() {
    let (mut pipeline, provider_id) = harness(true);
    let (req, _disconnect) = make_request(ComboId(1));
    let mut ctx = PipelineContext::new(req, pipeline.clone());
    ctx.current_target = Some(resolved_target(TargetFormat::Openai));

    // The REAL FormattingStage wraps the body using generation 1.
    let chain = crate::pipeline_chain![FormattingStage];
    let _ = chain.execute_nested(&mut ctx).await;
    let body = ctx
        .body_bytes
        .clone()
        .expect("formatted body survives the stage");
    let wrapped: serde_json::Value = serde_json::from_slice(&body).unwrap();

    // It must have gone through the shared resolver, not a fresh lookup.
    assert_eq!(
        ctx.req.pinned_adapters.lock().len(),
        1,
        "FormattingStage must pin through pinned_adapter()"
    );

    // The registry is refreshed between the wrap and the dispatch.
    pipeline.config.adapters = Arc::new(vec![adapter_generation(false)]);

    let pinned = pinned_adapter(&ctx.req, &pipeline.config.adapters, &provider_id)
        .expect("provider still present after the refresh");
    let target_format = ctx.target_format.expect("target format resolved");
    let model_id = openproxy_types::ModelId::new("m");
    let resolved = ctx.current_target.as_ref().expect("current target");

    // Headers, chat URL and finalization all observe the generation that
    // wrapped the body — not the refreshed one.
    let headers = pinned.build_headers("", target_format, &model_id);
    assert!(
        headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("x-openproxy-stream") && v == "false"),
        "build_headers must use the pinned generation, got {headers:?}"
    );
    assert_eq!(
        pinned.build_chat_url_for_account(target_format, &model_id, "a1"),
        adapter_generation(true).build_chat_url_for_account(target_format, &model_id, "a1"),
        "the chat URL must be built from the pinned generation"
    );
    let mut finalized_headers = Vec::new();
    let finalized = pinned
        .finalize_request(
            body.clone(),
            &mut finalized_headers,
            target_format,
            &model_id,
            resolved,
        )
        .expect("finalize_request");
    assert_eq!(finalized, body, "identity finalize keeps the body");

    // And the body itself stays coherent with the pinned policy.
    assert_eq!(pinned.forced_streaming(), Some(false));
    assert_eq!(wrapped["stream"], serde_json::json!(false));

    // A fresh request observes the new generation.
    let (next_req, _disconnect) = make_request(ComboId(1));
    let next_ctx = PipelineContext::new(next_req, pipeline.clone());
    let fresh =
        pinned_adapter(&next_ctx.req, &pipeline.config.adapters, &provider_id).expect("fresh pin");
    assert_eq!(fresh.forced_streaming(), Some(true));
    assert_eq!(
        fresh.build_chat_url_for_account(target_format, &model_id, "a1"),
        adapter_generation(false).build_chat_url_for_account(target_format, &model_id, "a1")
    );
}

#[tokio::test]
async fn retries_and_races_reuse_the_pin_of_the_original_request() {
    let (mut pipeline, provider_id) = harness(true);
    let (req, _disconnect) = make_request(ComboId(1));
    let ctx = PipelineContext::new(req.clone(), pipeline.clone());
    let pinned = pinned_adapter(&ctx.req, &pipeline.config.adapters, &provider_id)
        .expect("pinned for the original request");

    // A retry and each racer clone the request; the map travels with it.
    let retry = req.clone();
    let race_a = req.clone();
    let race_b = req.clone();
    assert!(Arc::ptr_eq(&req.pinned_adapters, &retry.pinned_adapters));
    assert!(Arc::ptr_eq(&retry.pinned_adapters, &race_a.pinned_adapters));
    assert!(Arc::ptr_eq(
        &race_a.pinned_adapters,
        &race_b.pinned_adapters
    ));

    // Even after a refresh, every clone resolves to the same instance.
    pipeline.config.adapters = Arc::new(vec![adapter_generation(false)]);
    for (label, clone) in [("retry", &retry), ("race_a", &race_a), ("race_b", &race_b)] {
        let resolved =
            pinned_adapter(clone, &pipeline.config.adapters, &provider_id).expect("pin survives");
        assert!(
            Arc::ptr_eq(&pinned, &resolved),
            "{label} must reuse the pinned adapter, not a refreshed one"
        );
        assert_eq!(resolved.forced_streaming(), Some(false));
    }
}

#[tokio::test]
async fn each_provider_of_a_request_gets_its_own_pin() {
    let conn = test_connection();
    let master = Arc::new(openproxy_db::MasterKey::generate().unwrap());
    let mut config = test_config(master);
    let mut adapters = vec![adapter_generation(true)];
    adapters.push(ProviderAdapterEnum::Custom(Box::new(
        CustomAdapter::from_config(ProviderAdapterConfig {
            id: ProviderId::new("other"),
            name: "Kevin Ryan".into(),
            base_url: "http://127.0.0.1:9".into(),
            auth_type: AuthType::None,
            format: ProviderFormat::Openai,
            extra_headers: Vec::new(),
            anonymous_fallback: false,
            rate_limit_scope: "none".into(),
        }),
    )));
    config.adapters = Arc::new(adapters);
    let pipeline = crate::Pipeline::new(conn, config);
    let (req, _disconnect) = make_request(ComboId(1));

    let first = pinned_adapter(
        &req,
        &pipeline.config.adapters,
        &ProviderId::new(PROVIDER_ID),
    )
    .expect("first provider pinned");
    let second = pinned_adapter(&req, &pipeline.config.adapters, &ProviderId::new("other"))
        .expect("second provider pinned");
    assert!(!Arc::ptr_eq(&first, &second));

    // An unknown provider fails the same way as before and pins nothing.
    assert!(
        pinned_adapter(&req, &pipeline.config.adapters, &ProviderId::new("missing")).is_none(),
        "an unknown provider must still resolve to None"
    );
    assert_eq!(req.pinned_adapters.lock().len(), 2);
}
