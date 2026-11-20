//! Tests de wiring del dispatcher: invariantes cross-submódulo que solo
//! son observables con la composición completa de
//! `dispatcher::{fail, rotation, proxy, stream, unary, horde, mod}`.
//!
//! `super::*` resuelve contra el orquestador porque el archivo se declara
//! como `#[cfg(test)] mod tests;` desde `dispatcher/mod.rs`.

use super::stream_tests::{
    build_dispatcher_for_stream_test, make_stream_pipeline_request, make_stream_test_entities,
    seed_stream_provider,
};
use super::types::DispatchContext;

/// DB+pool pair compatible con `UpstreamDispatcher::new`.
pub(super) fn fresh_pool() -> (
    openproxy_db::DbPool,
    std::sync::Arc<parking_lot::Mutex<rusqlite::Connection>>,
    std::path::PathBuf,
) {
    let pool =
        openproxy_db::DbPool::test_pool_with_prefix("openproxy-wiring-test").expect("open pool");
    let extra = pool.open_connection().expect("open extra connection");
    let conn = std::sync::Arc::new(parking_lot::Mutex::new(extra));
    let path = pool.path().to_path_buf();
    (pool, conn, path)
}

#[tokio::test(flavor = "multi_thread")]
async fn handle_non_2xx_response_wires_is_hard_skip_for_validation_required() {
    let (_pool, conn_arc, _path) = fresh_pool();
    let provider_id = "wired-test";
    seed_stream_provider(&conn_arc, provider_id, "https://example.com");
    let (_pid, model, target, combo) = make_stream_test_entities(provider_id, "g-2.5");
    let dispatcher = build_dispatcher_for_stream_test(std::sync::Arc::clone(&conn_arc));

    let started = std::time::Instant::now();
    let dctx = DispatchContext {
        attempt: 1,
        race_size: 1,
        started,
        model: &model,
        proxy_url: None,
        proxy_status: None,
    };

    let mut req = make_stream_pipeline_request("g-2.5", false, None);
    std::sync::Arc::make_mut(&mut req.openai_request).messages =
        vec![openproxy_types::OpenAIMessage {
            role: "user".into(),
            content: Some(serde_json::Value::String("hi".into())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::new(),
        }];

    // 403 + VALIDATION_REQUIRED no debe penalizar la cuenta.
    let result = dispatcher
        .handle_non_2xx_response(
            403,
            None,
            r#"{"error":{"code":"VALIDATION_REQUIRED"}}"#.to_string(),
            req,
            &combo,
            &target,
            &model,
            &dctx,
            42,
            Some(10),
        )
        .await;

    let err = result
        .error
        .expect("non-2xx response must produce an error");
    assert!(
        err.is_hard_skip(),
        "audit #1: 403 VALIDATION_REQUIRED must yield is_hard_skip=true, got error={err:?}"
    );
    assert_eq!(
        err.upstream_error_class(),
        Some(openproxy_types::UpstreamErrorClass::ValidationRequired)
    );
}

#[test]
fn test_translate_non_streaming_body_commandcode_raw_chunk_and_full_response() {
    let mut pipeline_req =
        make_stream_pipeline_request("meta/muse-spark-1.3-contributor", false, None);
    pipeline_req.combo_id = openproxy_types::ComboId(0);

    let raw_chunk = serde_json::json!({
        "type": "text-delta",
        "text": "Hello world from CommandCode"
    });
    let res = super::unary::translate_non_streaming_body(
        openproxy_types::TargetFormat::CommandCodeGo,
        &raw_chunk,
        &pipeline_req,
    )
    .expect("must translate raw CommandCode chunk");
    assert_eq!(
        res.choices[0]
            .message
            .content
            .as_ref()
            .and_then(serde_json::Value::as_str),
        Some("Hello world from CommandCode")
    );

    let full_openai = serde_json::json!({
        "id": "chatcmpl_test",
        "object": "chat.completion",
        "created": 1234567,
        "model": "meta/muse-spark-1.3-contributor",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "Full response"
            },
            "finish_reason": "stop"
        }]
    });
    let res2 = super::unary::translate_non_streaming_body(
        openproxy_types::TargetFormat::CommandCodeGo,
        &full_openai,
        &pipeline_req,
    )
    .expect("must deserialize valid OpenAI shape");
    assert_eq!(
        res2.choices[0]
            .message
            .content
            .as_ref()
            .and_then(serde_json::Value::as_str),
        Some("Full response")
    );
}

/// OpenAI-dialect body helper: a complete response with the given usage block.
fn openai_body_with_usage(usage: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl_usage",
        "object": "chat.completion",
        "created": 1,
        "model": "gpt-5.4",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }],
        "usage": usage
    })
}

#[test]
fn test_translate_openai_body_reads_input_output_aliases_into_usage() {
    // The presence probe saw input_tokens/output_tokens as present while
    // `OpenAIUsage` only deserializes the canonical names, so the translator
    // used to return 0/0/26 with the fields falsely marked observed.
    let req = make_stream_pipeline_request("gpt-5.4", false, None);
    let body = openai_body_with_usage(serde_json::json!({
        "input_tokens": 17,
        "output_tokens": 9,
        "total_tokens": 26
    }));
    let (resp, presence) = super::unary::translate_non_streaming_body_with_presence(
        openproxy_types::TargetFormat::Openai,
        &body,
        &req,
    )
    .expect("translate openai body");
    let usage = resp.usage.expect("usage must be present");
    assert_eq!(
        (
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.total_tokens
        ),
        (17, 9, 26),
        "aliases must be read, not defaulted to zero"
    );
    assert_eq!(
        presence,
        openproxy_types::UsagePresence {
            prompt: true,
            completion: true,
            total: true,
            cached: false,
        }
    );
}

#[test]
fn test_translate_openai_body_prefers_canonical_names_when_aliases_coexist() {
    // Both spellings present: canonical names win, and the response must not
    // be rejected (a `#[serde(alias)]` on the usage struct raises
    // `duplicate field` here).
    let req = make_stream_pipeline_request("gpt-5.4", false, None);
    let body = openai_body_with_usage(serde_json::json!({
        "prompt_tokens": 11,
        "input_tokens": 17,
        "completion_tokens": 4,
        "output_tokens": 9,
        "total_tokens": 15
    }));
    let (resp, presence) = super::unary::translate_non_streaming_body_with_presence(
        openproxy_types::TargetFormat::Openai,
        &body,
        &req,
    )
    .expect("coexisting canonical+alias must parse");
    let usage = resp.usage.expect("usage must be present");
    assert_eq!(
        (
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.total_tokens
        ),
        (11, 4, 15)
    );
    assert!(presence.prompt && presence.completion && presence.total);
}

#[test]
fn test_translate_openai_body_keeps_explicit_zero_and_estimates_missing_only() {
    // `Some(0)` is an upstream observation, not absence: it must survive both
    // the translator and the fill step, and only the missing field is
    // estimated.
    let mut req = make_stream_pipeline_request("gpt-5.4", false, None);
    req.openai_request = std::sync::Arc::new(openproxy_types::OpenAIRequest {
        messages: vec![openproxy_types::OpenAIMessage {
            role: "user".to_string(),
            content: Some(serde_json::Value::String("hi".to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        }],
        ..(*req.openai_request).clone()
    });
    let body = openai_body_with_usage(serde_json::json!({
        "prompt_tokens": 0,
        "total_tokens": 0
    }));
    let (mut resp, presence) = super::unary::translate_non_streaming_body_with_presence(
        openproxy_types::TargetFormat::Openai,
        &body,
        &req,
    )
    .expect("translate openai body");
    assert!(presence.prompt, "an explicit zero is still present");
    assert!(!presence.completion, "completion was not reported");
    assert_eq!(resp.usage.as_ref().expect("usage").prompt_tokens, 0);

    let fill = super::unary::ensure_usage_populated(
        &mut resp,
        &req.openai_request,
        Some("gpt-5.4"),
        presence,
        openproxy_compression::token_estimate::PromptRail::OpenAiChat,
    );
    let usage = resp.usage.expect("usage");
    assert_eq!(
        usage.prompt_tokens, 0,
        "the observed zero must reach the wire unchanged"
    );
    assert!(
        usage.completion_tokens > 0,
        "only the missing completion is estimated"
    );
    assert_eq!(usage.total_tokens, usage.completion_tokens);
    assert_eq!(fill.estimated, (false, true));
    let observed = fill.observed.expect("observed snapshot");
    assert_eq!(
        observed.prompt,
        Some(0),
        "the recorded zero is an observation"
    );
    assert_eq!(observed.completion, None);
}

#[test]
fn test_translate_openai_body_null_completion_is_absent_not_a_parse_error() {
    // R3 blocker: a canonical counter arriving as `null` aborted the whole
    // response before the optional extraction could classify it as absent.
    let req = make_stream_pipeline_request("gpt-5.4", false, None);
    let body = openai_body_with_usage(serde_json::json!({
        "prompt_tokens": 17,
        "completion_tokens": null,
        "total_tokens": 17
    }));

    let (resp, presence) = super::unary::translate_non_streaming_body_with_presence(
        openproxy_types::TargetFormat::Openai,
        &body,
        &req,
    )
    .expect("a null canonical counter must parse as absent");
    let usage = resp.usage.expect("usage must be present");
    assert_eq!(
        (
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.total_tokens
        ),
        (17, 0, 17)
    );
    assert_eq!(
        presence,
        openproxy_types::UsagePresence {
            prompt: true,
            completion: false,
            total: true,
            cached: false,
        },
        "null completion is absent, not an observed zero"
    );

    // The presence-less entry point must accept the same body.
    let legacy = super::unary::translate_non_streaming_body(
        openproxy_types::TargetFormat::Openai,
        &body,
        &req,
    )
    .expect("the public translator must accept null usage counters");
    assert_eq!(legacy.usage.expect("legacy usage").prompt_tokens, 17);
}

#[test]
fn test_translate_openai_body_null_canonical_prompt_falls_back_to_input_tokens() {
    let req = make_stream_pipeline_request("gpt-5.4", false, None);
    let body = openai_body_with_usage(serde_json::json!({
        "prompt_tokens": null,
        "input_tokens": 17,
        "completion_tokens": 9,
        "total_tokens": 26
    }));

    let (resp, presence) = super::unary::translate_non_streaming_body_with_presence(
        openproxy_types::TargetFormat::Openai,
        &body,
        &req,
    )
    .expect("null canonical prompt must fall back to the valid alias");
    let usage = resp.usage.expect("usage must be present");
    assert_eq!(
        (
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.total_tokens
        ),
        (17, 9, 26),
        "the alias is a valid observation when the canonical name is null"
    );
    assert_eq!(
        presence,
        openproxy_types::UsagePresence {
            prompt: true,
            completion: true,
            total: true,
            cached: false,
        }
    );
}

#[test]
fn test_translate_openai_body_explicit_zero_usage_is_observed() {
    let req = make_stream_pipeline_request("gpt-5.4", false, None);
    let body = openai_body_with_usage(serde_json::json!({
        "prompt_tokens": 0,
        "completion_tokens": 0,
        "total_tokens": 0
    }));

    let (resp, presence) = super::unary::translate_non_streaming_body_with_presence(
        openproxy_types::TargetFormat::Openai,
        &body,
        &req,
    )
    .expect("explicit zeros must parse");
    let usage = resp.usage.expect("usage must be present");
    assert_eq!(
        (
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.total_tokens
        ),
        (0, 0, 0)
    );
    assert_eq!(
        presence,
        openproxy_types::UsagePresence {
            prompt: true,
            completion: true,
            total: true,
            cached: false,
        }
    );
}

#[test]
fn test_translate_openai_body_invalid_field_outside_usage_still_fails() {
    // Only `usage` is excluded from the strict contract: a malformed member
    // anywhere else must keep rejecting the body.
    let req = make_stream_pipeline_request("gpt-5.4", false, None);
    let mut body = openai_body_with_usage(serde_json::json!({
        "prompt_tokens": 17,
        "completion_tokens": 0,
        "total_tokens": 17
    }));
    body["choices"] = serde_json::json!("not-an-array");

    let err = super::unary::translate_non_streaming_body_with_presence(
        openproxy_types::TargetFormat::Openai,
        &body,
        &req,
    )
    .expect_err("invalid choices must still fail");
    assert!(
        err.to_string().contains("parse openai response"),
        "the failure must keep the strict-parse context, got: {err}"
    );
}
