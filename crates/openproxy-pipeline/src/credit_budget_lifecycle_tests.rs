//! Lifecycle regressions for the strict prepaid budget and prepaid streams.
//!
//! These drive the real `Pipeline::run` against a local SSE fixture and a real
//! SQLite ledger. They assert what actually reaches the database: no upstream
//! hit when the maximum does not fit, a normal unary/stream bill, a crash after
//! the first chunk charged only for delivered bytes, unpriced models failing
//! closed, free keys untouched and `execute_test_target` not bypassing the gate.

use crate::credits_gate::tests::{Harness, Upstream, UpstreamMode};
use std::sync::atomic::Ordering;
use std::time::Duration;

fn start_budget_harness(
    upstream: &Upstream,
    model_id: &str,
    combo_price: Option<(f64, f64)>,
    grant: Option<i64>,
    with_worker: bool,
) -> Harness {
    Harness::start_with_options(upstream, Some(model_id), combo_price, true, grant, with_worker)
}

/// `openai/gpt-4o` is in the static catalog (2.5 input / 10 output USD per
/// million), so a fixture provider with that model id prices without a DB row.
const STATIC_MODEL: &str = "openai/gpt-4o";

#[tokio::test]
async fn the_last_draw_admits_a_request_the_maximum_exceeds() {
    let upstream = Upstream::start(UpstreamMode::BudgetOk).await;
    let harness = start_budget_harness(&upstream, STATIC_MODEL, Some((2.5, 10.0)), Some(100), true);

    let result = harness
        .run_with_timeout(harness.request_with_max_tokens(Some(64)))
        .await;

    assert_eq!(
        result.status_code, 200,
        "a positive balance is always admitted (last-draw): {:?}",
        result.error
    );
    assert_eq!(
        harness.hits.load(Ordering::SeqCst),
        1,
        "the request reached the upstream"
    );
    assert_eq!(
        harness.balance(),
        0,
        "the debit clamped to the balance: the plan closes at exactly 0"
    );
}

#[tokio::test]
async fn affordable_request_is_dispatched_and_billed_once() {
    let upstream = Upstream::start(UpstreamMode::BudgetOk).await;
    let harness = start_budget_harness(
        &upstream,
        STATIC_MODEL,
        Some((2.5, 10.0)),
        Some(10_000_000),
        true,
    );

    let req = harness.request_with_max_tokens(Some(64));
    let request_id = req.request_id.to_string();
    let result = harness.run_with_timeout(req).await;

    assert_eq!(result.status_code, 200, "{:?}", result.error);
    assert_eq!(harness.hits.load(Ordering::SeqCst), 1);
    let (client_response, debited, cost) = harness
        .usage_row(&request_id)
        .expect("usage row was recorded");
    assert_eq!(client_response, 1);
    assert_eq!(debited, 1);
    // 100 prompt tokens at 2.5 USD/M = 250 units, plus the delivered "ok"
    // completion (the upstream reported 0, so the recorder estimates 1 token
    // at 10 USD/M = 10 units): 260 units.
    assert!((cost - 0.00026).abs() < 1e-12, "unexpected cost {cost}");
    assert_eq!(harness.balance(), 10_000_000 - 260);
}

#[tokio::test]
async fn streaming_commits_delivery_evidence_and_bills_the_actual_usage() {
    let upstream = Upstream::start(UpstreamMode::BudgetOk).await;
    let harness = start_budget_harness(
        &upstream,
        STATIC_MODEL,
        Some((2.5, 10.0)),
        Some(10_000_000),
        true,
    );

    let (req, mut chunks) = harness.streaming_request_with_max_tokens(Some(64));
    let request_id = req.request_id.to_string();
    let result = harness.run_with_timeout(req).await;
    assert_eq!(result.status_code, 200, "{:?}", result.error);
    assert_eq!(harness.hits.load(Ordering::SeqCst), 1);

    let mut wire = Vec::new();
    while let Ok(chunk) = chunks.try_recv() {
        wire.extend_from_slice(&chunk);
    }
    let wire = String::from_utf8_lossy(&wire);
    assert!(wire.contains("ok"), "stream content reached the client");
    assert!(
        wire.contains("[DONE]"),
        "stream terminator reached the client"
    );

    let (client_response, debited, cost) =
        harness.usage_row(&request_id).expect("streaming usage row");
    assert_eq!(client_response, 1);
    assert_eq!(debited, 1);
    // Same arithmetic as the unary test: 250 prompt + 10 for the estimated
    // 1-token completion delivered before the usage frame reported 0.
    assert!((cost - 0.00026).abs() < 1e-12, "unexpected cost {cost}");

    let (generated_units, delivery_started, reserved_units, state, outcome, _charged) = harness
        .reservation(&request_id)
        .expect("prepaid stream must persist a reservation");
    assert!(
        generated_units > 0,
        "the reservation carries delivered-cost evidence"
    );
    assert_eq!(
        delivery_started, 1,
        "delivery is recorded after the first enqueued chunk"
    );
    assert!(reserved_units <= 10_000_000);
    assert_eq!(state, "settled");
    assert_eq!(outcome.as_deref(), Some("billed"));
    assert_eq!(harness.balance(), 10_000_000 - 260);
}

#[tokio::test]
async fn a_crashed_stream_is_charged_from_its_delivered_checkpoint() {
    let upstream = Upstream::start(UpstreamMode::HangAfterFirstChunk).await;
    let harness = start_budget_harness(
        &upstream,
        STATIC_MODEL,
        Some((2.5, 10.0)),
        Some(10_000_000),
        true,
    );

    let (req, mut chunks) = harness.streaming_request_with_max_tokens(Some(10_000));
    let request_id = req.request_id.to_string();
    let pipeline = harness.pipeline.clone();
    let task = tokio::spawn(async move { pipeline.run(req).await });

    // The first chunk reaching the client is the delivery point. The sink
    // enqueues first and then checkpoints, so wait for the durable flag before
    // simulating the crash.
    let first = tokio::time::timeout(Duration::from_secs(5), chunks.recv())
        .await
        .expect("the first chunk never reached the client")
        .expect("stream channel closed");
    assert!(
        String::from_utf8_lossy(&first).contains("partial"),
        "unexpected first chunk: {first:?}"
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if harness
            .reservation(&request_id)
            .is_some_and(|row| row.1 == 1)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the delivered chunk was never checkpointed"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Crash the request between the first chunk and `record_streaming_success`:
    // no usage row exists, and the process (task) is gone.
    task.abort();
    assert!(task.await.expect_err("aborted task").is_cancelled());

    let (generated_units, delivery_started, reserved_units, state, _outcome, _charged) = harness
        .reservation(&request_id)
        .expect("delivery evidence must survive the crash");
    assert!(generated_units > 0, "the delivered cost was checkpointed");
    assert_eq!(delivery_started, 1);
    assert!(reserved_units > 0, "admission recorded a budget snapshot");
    assert_eq!(state, "held", "the owner never settled, so it stays open");
    assert_eq!(harness.usage_row(&request_id), None, "no usage row landed");

    // The next admission of this key is the recovery point: it must charge the
    // delivered-cost checkpoint instead of treating the crash as \"nothing delivered\".
    let next = harness.request_with_max_tokens(Some(64));
    let admission = harness.pipeline.admit_credit_request(&next).await;
    drop(admission);

    let (_, _, _, state, outcome, charged_after) = harness
        .reservation(&request_id)
        .expect("the recovered intent still exists");
    assert_eq!(state, "settled");
    assert_eq!(outcome.as_deref(), Some("recovered_partial"));
    assert_eq!(
        charged_after, generated_units,
        "recovery charges exactly the delivered-cost checkpoint"
    );
    assert!(
        charged_after < 5_000,
        "a crash must not charge the unproduced output ceiling (charged {charged_after})"
    );
    assert_eq!(
        harness.balance(),
        10_000_000 - charged_after,
        "the partial delivery was not free"
    );
}

#[tokio::test]
async fn unpriced_model_fails_closed_before_dispatch() {
    let upstream = Upstream::start(UpstreamMode::BudgetOk).await;
    let harness = start_budget_harness(
        &upstream,
        "no/such-model-budget-fixture",
        None,
        Some(10_000_000),
        true,
    );

    let result = harness
        .run_with_timeout(harness.request_with_max_tokens(Some(16)))
        .await;
    assert_ne!(result.status_code, 200, "a missing price cannot be billed");
    assert!(result.status_code >= 500, "got {}", result.status_code);
    assert_eq!(harness.hits.load(Ordering::SeqCst), 0, "no upstream hit");
}

#[tokio::test]
async fn free_keys_keep_full_access_even_with_unpriced_models() {
    let upstream = Upstream::start(UpstreamMode::BudgetOk).await;
    let harness =
        start_budget_harness(&upstream, "no/such-model-budget-fixture", None, None, true);

    let result = harness
        .run_with_timeout(harness.request_with_max_tokens(None))
        .await;
    assert_eq!(
        result.status_code, 200,
        "a key without ledger rows is not budget-governed: {:?}",
        result.error
    );
    assert_eq!(harness.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn execute_test_target_runs_the_last_draw() {
    let upstream = Upstream::start(UpstreamMode::BudgetOk).await;
    let harness = start_budget_harness(&upstream, STATIC_MODEL, Some((2.5, 10.0)), Some(100), true);

    let req = harness.request_with_max_tokens(Some(64));
    let resolved = crate::context::ResolvedTarget::test("mock-openai", STATIC_MODEL);
    let result = harness.pipeline.execute_test_target(req, resolved).await;

    assert_eq!(
        result.status_code, 200,
        "admin test traffic obeys the same last-draw admission: {:?}",
        result.error
    );
    assert_eq!(harness.hits.load(Ordering::SeqCst), 1);
    // The test-target path dispatches without settling a usage row, so no
    // debit (clamped or not) is asserted here; the admission itself is the
    // last-draw contract under test.
}

#[tokio::test]
async fn aborted_stream_still_leaves_the_lane_free() {
    let upstream = Upstream::start(UpstreamMode::Hang).await;
    let harness = start_budget_harness(
        &upstream,
        STATIC_MODEL,
        Some((2.5, 10.0)),
        Some(10_000_000),
        true,
    );

    let pipeline = harness.pipeline.clone();
    let (req, _chunks) = harness.streaming_request_with_max_tokens(Some(64));
    let task = tokio::spawn(async move { pipeline.run(req).await });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while harness.hits.load(Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the hanging upstream never saw the request"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    task.abort();
    assert!(task.await.expect_err("aborted task").is_cancelled());

    harness.assert_lane_free().await;
}
