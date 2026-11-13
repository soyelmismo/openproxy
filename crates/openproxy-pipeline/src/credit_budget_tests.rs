//! Arithmetic regressions for the strict prepaid budget.
//!
//! The pure tests pin the one-sided cost bound: payload bytes are the prompt
//! ceiling, output needs an explicit or model-declared limit, cached rates use
//! the higher price, and missing/non-finite prices or media modalities fail
//! closed. Lifecycle coverage lives in `credit_budget_lifecycle_tests.rs`.

use super::*;
use openproxy_db::pricing::{Price, PriceKind};
use openproxy_types::combos::ComboTarget;
use openproxy_types::ids::ComboId;
use openproxy_types::models::Model;
use std::sync::Arc;

fn test_combo() -> Combo {
    Combo::default()
}

fn test_target(thinking_effort: Option<&str>) -> ComboTarget {
    ComboTarget {
        thinking_effort: thinking_effort.map(str::to_string),
        ..ComboTarget::default()
    }
}

fn test_model(max_output_tokens: Option<i64>) -> Model {
    Model {
        model_id: "test-model".into(),
        max_output_tokens,
        ..Model::default()
    }
}

// Deliberately `Option`: every call site passes the value straight into the
// price-taking APIs under test, which expect `Option<Price>`.
#[allow(clippy::unnecessary_wraps)]
fn priced(input: f64, output: f64, cached: Option<f64>) -> Option<Price> {
    Some(Price {
        input_per_1m: input,
        output_per_1m: output,
        cached_per_1m: cached,
        ..Price::default()
    })
}

fn request_with_max(max_tokens: Option<u32>) -> PipelineRequest {
    let (mut req, _cancel) = crate::test_utils::make_request(ComboId(1));
    let inner = Arc::make_mut(&mut req.openai_request);
    inner.max_tokens = max_tokens;
    inner.messages[0].content = Some(serde_json::Value::String("hi".to_string()));
    req
}

// ── arithmetic ──────────────────────────────────────────────────────

#[test]
fn input_is_bounded_by_payload_bytes_not_by_a_char_estimate() {
    let req = request_with_max(None);
    let quote = quote(
        &req,
        &test_combo(),
        &test_target(None),
        &test_model(None),
        1_000,
        priced(3.0, 0.0, None),
    )
    .expect("free output needs no ceiling");

    assert_eq!(quote.prompt_tokens, 1_000);
    assert_eq!(quote.output_tokens, 0);
    // 1000 bytes * 3.0 / 1e6 = 0.003 USD = 3000 units (+1 headroom).
    assert_eq!(quote.units, 3_001);
}

#[test]
fn missing_price_is_unverifiable_and_fails_closed() {
    let req = request_with_max(Some(16));
    let error = quote(
        &req,
        &test_combo(),
        &test_target(None),
        &test_model(None),
        128,
        None,
    )
    .expect_err("no price means no verifiable maximum");
    assert!(
        error.to_string().contains("no price"),
        "unexpected error: {error}"
    );
}

#[test]
fn non_finite_rates_fail_closed() {
    for price in [
        priced(f64::NAN, 0.0, None),
        priced(1.0, f64::INFINITY, None),
        priced(-1.0, 0.0, None),
        priced(1.0, 2.0, Some(f64::NAN)),
    ] {
        let req = request_with_max(Some(16));
        let error = quote(
            &req,
            &test_combo(),
            &test_target(None),
            &test_model(None),
            128,
            price,
        )
        .expect_err("a non-finite rate cannot bound a cost");
        assert!(error.to_string().contains("not a finite"));
    }
}

#[test]
fn priced_output_without_a_ceiling_fails_closed() {
    let req = request_with_max(None);
    let error = quote(
        &req,
        &test_combo(),
        &test_target(None),
        &test_model(None),
        128,
        priced(1.0, 2.0, None),
    )
    .expect_err("output is not free and has no verifiable ceiling");
    assert!(error.to_string().contains("no explicit max_tokens"));
}

#[test]
fn model_ceiling_bounds_output_when_the_client_omits_it() {
    let req = request_with_max(None);
    let quote = quote(
        &req,
        &test_combo(),
        &test_target(None),
        &test_model(Some(100)),
        128,
        priced(1.0, 2.0, None),
    )
    .expect("model ceiling is a verifiable bound");
    assert_eq!(quote.output_tokens, 100);
}

#[test]
fn choice_multiplier_multiplies_the_output_ceiling() {
    let mut req = request_with_max(Some(10));
    Arc::make_mut(&mut req.openai_request)
        .extra
        .insert("n".to_string(), serde_json::json!(3));
    let quote = quote(
        &req,
        &test_combo(),
        &test_target(None),
        &test_model(None),
        128,
        priced(1.0, 2.0, None),
    )
    .expect("bounded request");
    assert_eq!(quote.output_tokens, 30);
}

#[test]
fn cached_input_is_priced_at_the_higher_rate() {
    let req = request_with_max(None);
    let quote = quote(
        &req,
        &test_combo(),
        &test_target(None),
        &test_model(None),
        1_000_000,
        priced(1.0, 0.0, Some(10.0)),
    )
    .expect("bounded input");
    // 1M bytes at the cached rate (10 USD/M) = 10 USD = 10_000_000 units (+1).
    assert_eq!(quote.units, 10_000_001);
}

#[test]
fn multimodal_input_fails_closed() {
    let mut req = request_with_max(Some(16));
    Arc::make_mut(&mut req.openai_request).messages[0].content = Some(serde_json::json!([
        {"type": "text", "text": "describe"},
        {"type": "image_url", "image_url": {"url": "https://example.test/a.png"}}
    ]));
    let error = quote(
        &req,
        &test_combo(),
        &test_target(None),
        &test_model(None),
        128,
        priced(1.0, 2.0, None),
    )
    .expect_err("image pricing is not expressible as token rates");
    assert!(error.to_string().contains("image"));
}

#[test]
fn reasoning_requires_the_model_declared_ceiling() {
    let mut req = request_with_max(Some(10));
    Arc::make_mut(&mut req.openai_request)
        .extra
        .insert("reasoning_effort".to_string(), serde_json::json!("high"));
    let error = quote(
        &req,
        &test_combo(),
        &test_target(None),
        &test_model(None),
        128,
        priced(1.0, 2.0, None),
    )
    .expect_err("client max_tokens does not bound hidden reasoning tokens");
    assert!(error.to_string().contains("model-declared output ceiling"));

    let quote = quote(
        &req,
        &test_combo(),
        &test_target(None),
        &test_model(Some(100)),
        128,
        priced(1.0, 2.0, None),
    )
    .expect("model ceiling bounds reasoning");
    assert_eq!(quote.output_tokens, 100);
}

#[test]
fn audio_price_kind_is_not_billable_by_chat_token_rates() {
    let req = request_with_max(Some(16));
    let price = Some(Price {
        kind: PriceKind::Audio,
        audio_per_hour: Some(1.0),
        ..Price::default()
    });
    let error = quote(
        &req,
        &test_combo(),
        &test_target(None),
        &test_model(None),
        128,
        price,
    )
    .expect_err("audio pricing has no token bound");
    assert!(
        error
            .to_string()
            .contains("not billable by chat token rates")
    );
}
