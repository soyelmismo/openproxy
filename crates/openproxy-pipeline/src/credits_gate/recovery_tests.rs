use super::*;

#[tokio::test]
async fn aborted_request_does_not_hold_the_lane() {
    let upstream = Upstream::start(UpstreamMode::Hang).await;
    let harness = Harness::start(&upstream, Some(KEY_GRANT_UNITS), true);

    let pipeline = harness.pipeline.clone();
    let req = harness.request().0;
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
    assert_eq!(
        harness.balance(),
        KEY_GRANT_UNITS,
        "an aborted response never bills"
    );
}

#[tokio::test]
async fn key_without_ledger_rows_keeps_full_concurrency() {
    let upstream = Upstream::start(UpstreamMode::Ok).await;
    let harness = Harness::start(&upstream, None, true);

    let (a, b) = tokio::join!(
        harness.run_with_timeout(harness.request().0),
        harness.run_with_timeout(harness.request().0)
    );
    assert_eq!(a.status_code, 200, "{:?}", a.error);
    assert_eq!(b.status_code, 200, "{:?}", b.error);
    // Admission returns before it ever touches the lane table for a key with no
    // ledger rows, so no lane can exist for it; both requests went out in
    // parallel because nothing serialized them.
    let governed = {
        let conn = harness.pool.writer();
        openproxy_db::credits_ledger::is_credit_based(&conn, harness.key_id).expect("probe")
    };
    assert!(!governed, "fixture key has no ledger rows");
    assert_eq!(
        harness.hits.load(Ordering::SeqCst),
        2,
        "both requests were dispatched concurrently"
    );
}

#[tokio::test]
async fn admission_applies_a_journaled_debit_left_by_an_aborted_settlement() {
    let upstream = Upstream::start(UpstreamMode::Ok).await;
    // No worker: the debit stays journaled, exactly like a request whose task
    // was cancelled between recording usage and settling it.
    let harness = Harness::start(&upstream, Some(KEY_GRANT_UNITS), false);

    let usage_request_id = RequestId::new();
    harness.record_delivered_usage(usage_request_id);
    assert_eq!(
        harness.balance(),
        KEY_GRANT_UNITS,
        "recording usage alone does not debit"
    );
    harness.journal_mark(usage_request_id);

    // Admission must drain the journal before trusting the balance.
    let result = harness.run_with_timeout(harness.request().0).await;
    assert_eq!(
        result.status_code, 402,
        "the deferred debit must deny the next request: {:?}",
        result.error
    );
    assert_eq!(harness.balance(), 0);
    assert_eq!(
        harness
            .usage_row(&usage_request_id.to_string())
            .map(|row| row.1),
        Some(1),
        "the journaled usage row is now debited"
    );
    assert_eq!(
        harness.hits.load(Ordering::SeqCst),
        0,
        "denied before dispatch"
    );
}

// ── fail-closed admission ────────────────────────────────────────────

#[tokio::test]
async fn admission_is_denied_when_the_journal_cannot_be_drained() {
    let upstream = Upstream::start(UpstreamMode::Ok).await;
    let harness = Harness::start(&upstream, Some(KEY_GRANT_UNITS), false);

    // A durable debit that must be applied before any balance is trusted. With
    // the journal table gone the drain cannot run, so admission must refuse
    // rather than admit against a balance that still contains the unapplied
    // debit.
    let usage_request_id = RequestId::new();
    harness.record_delivered_usage(usage_request_id);
    harness.journal_mark(usage_request_id);
    {
        let conn = harness.pool.writer();
        conn.execute_batch("DROP TABLE usage_journal")
            .expect("drop journal");
    }

    let result = harness.run_with_timeout(harness.request().0).await;
    assert!(
        result.status_code >= 500,
        "a stale balance must fail closed, got {}: {:?}",
        result.status_code,
        result.error
    );
    assert_eq!(
        harness.hits.load(Ordering::SeqCst),
        0,
        "denied before dispatch"
    );
    assert_eq!(
        harness.balance(),
        KEY_GRANT_UNITS,
        "nothing was debited, so nothing may be spent"
    );
}

#[tokio::test]
async fn unposted_debit_is_posted_before_the_next_admission() {
    let upstream = Upstream::start(UpstreamMode::Ok).await;
    // No worker: the debit stays journaled until someone drains it.
    let harness = Harness::start(&upstream, Some(KEY_GRANT_UNITS), false);

    let usage_request_id = RequestId::new();
    harness.record_delivered_usage(usage_request_id);
    harness.journal_mark(usage_request_id);
    harness.pipeline.credits_gate().note_pending_debit(
        harness.db_id(),
        harness.key_id,
        PendingDebit::new(usage_request_id.to_string(), 1, harness.target_id),
    );

    // The next spend must see the charge: the admission posts it first, the
    // balance drops to zero and the request is refused.
    let result = harness.run_with_timeout(harness.request().0).await;
    assert_eq!(
        result.status_code, 402,
        "the pending debit must deny the next request: {:?}",
        result.error
    );
    assert_eq!(
        harness
            .usage_row(&usage_request_id.to_string())
            .map(|row| row.1),
        Some(1),
        "admission posted the pending debit"
    );
    assert!(
        harness.pending_debit().is_none(),
        "a posted debit must clear its marker"
    );
    assert_eq!(harness.balance(), 0);
    assert_eq!(
        harness.hits.load(Ordering::SeqCst),
        0,
        "denied before dispatch"
    );
}

#[tokio::test]
async fn unpostable_debit_keeps_the_key_closed() {
    let upstream = Upstream::start(UpstreamMode::Ok).await;
    let harness = Harness::start(&upstream, Some(KEY_GRANT_UNITS), false);

    let usage_request_id = RequestId::new();
    harness.record_delivered_usage(usage_request_id);
    harness.journal_mark(usage_request_id);
    harness.pipeline.credits_gate().note_pending_debit(
        harness.db_id(),
        harness.key_id,
        PendingDebit::new(usage_request_id.to_string(), 1, harness.target_id),
    );

    // Make the writer reject writes: the owed charge can no longer be posted.
    {
        let conn = harness.pool.writer();
        conn.execute_batch("PRAGMA query_only = ON")
            .expect("read-only writer");
    }

    let result = harness.run_with_timeout(harness.request().0).await;
    assert!(
        result.status_code >= 500,
        "an unpostable charge must fail closed, got {}: {:?}",
        result.status_code,
        result.error
    );
    assert_eq!(
        harness.hits.load(Ordering::SeqCst),
        0,
        "denied before dispatch"
    );
    assert!(
        harness.pending_debit().is_some(),
        "the marker must survive so the key stays closed until the charge lands"
    );
}

/// A stream that reached the client but whose usage row failed to record must
/// not close its durable intent as `billed`: that would make the delivery
/// free forever. The intent stays open, and the next admission reconciles it
/// from the delivered-output checkpoint.
#[tokio::test]
async fn delivered_without_a_usage_row_leaves_the_intent_open_for_recovery() {
    let upstream = Upstream::start(UpstreamMode::Ok).await;
    let harness = Harness::start(&upstream, Some(KEY_GRANT_UNITS), false);
    let (req, _cancel) = harness.request();
    let request_id = req.request_id.to_string();

    let lane = match harness.pipeline.admit_credit_request(&req).await {
        crate::credits_gate::CreditAdmission::Open(Some(lane)) => lane,
        crate::credits_gate::CreditAdmission::Open(None) => {
            panic!("a granted key is credit-governed and must admit")
        }
        crate::credits_gate::CreditAdmission::Denied(_) => {
            panic!("a granted key with a fresh balance must admit")
        }
    };

    let result = crate::PipelineResult {
        status_code: 200,
        error: None,
        final_response: None,
        attempts: 1,
        usage_tuple: None,
    };
    harness.pipeline.settle_credit_request(lane, &result).await;
    harness.assert_lane_free().await;

    fn intent_state(harness: &Harness, request_id: &str) -> (String, Option<String>) {
        harness
            .pool
            .writer()
            .query_row(
                "SELECT state, outcome FROM credit_reservations \
                 WHERE request_id = ?1 ORDER BY id DESC LIMIT 1",
                [request_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("durable intent row")
    }

    assert_eq!(
        intent_state(&harness, &request_id),
        ("held".into(), None),
        "a delivered response without a usage row must not close as billed"
    );

    // The dispatcher had checkpointed 400 units of delivered output before the
    // usage row was lost; recovery must charge exactly that, never the balance.
    {
        let conn = harness.pool.writer();
        openproxy_db::reservations::checkpoint(&conn, harness.key_id, &request_id, 1, 400)
            .expect("checkpoint the delivered output");
    }

    let before = harness.balance();
    match harness
        .pipeline
        .admit_credit_request(&harness.request().0)
        .await
    {
        crate::credits_gate::CreditAdmission::Open(_) => {}
        crate::credits_gate::CreditAdmission::Denied(_) => {
            panic!("recovery must admit again afterwards")
        }
    }
    assert_eq!(harness.balance(), before - 400);
    let (state, outcome) = intent_state(&harness, &request_id);
    assert_eq!(state, "settled");
    assert_eq!(outcome.as_deref(), Some("recovered_partial"));
    harness.assert_lane_free().await;
}

/// SECURITY REGRESSION (audit round 2, V-01/A5-1): a client that received part
/// of a stream and then closed the socket (499) must be billed for the output
/// it already received. The `CreditStreamGuard` checkpoints the cost of every
/// delivered chunk into `credit_reservations.generated_units`, and the crash
/// reconciliation bills exactly that evidence — the owner settlement of a
/// client disconnect must do the same, or "receive tokens, drop the socket"
/// becomes a free-output primitive that also bypasses the 5h/weekly windows
/// (their sums only count `client_response = 1` rows).
#[tokio::test]
async fn client_disconnect_mid_stream_bills_the_delivered_checkpoint() {
    let upstream = Upstream::start(UpstreamMode::Ok).await;
    let harness = Harness::start(&upstream, Some(KEY_GRANT_UNITS), false);
    let (req, _cancel) = harness.request();
    let request_id = req.request_id.to_string();

    let lane = match harness.pipeline.admit_credit_request(&req).await {
        crate::credits_gate::CreditAdmission::Open(Some(lane)) => lane,
        crate::credits_gate::CreditAdmission::Open(None) => {
            panic!("a granted key is credit-governed and must admit")
        }
        crate::credits_gate::CreditAdmission::Denied(_) => {
            panic!("a granted key with a fresh balance must admit")
        }
    };

    // The stream handed chunks to the client and the guard checkpointed their
    // cost (what `CreditStreamGuard` persists every CHECKPOINT_EVERY_BYTES).
    openproxy_db::reservations::checkpoint(
        &harness.pool.writer(),
        harness.key_id,
        &request_id,
        1,
        400,
    )
    .expect("checkpoint the delivered output");

    // The client closes the socket: the dispatcher settles a 499 with
    // Cancelled(ClientDisconnected). The tracker also recorded a failed usage
    // row (client_response = 0), which is what usage_tuple carries here.
    let result = crate::PipelineResult {
        status_code: 499,
        error: Some(CoreError::Cancelled(CancelReason::ClientDisconnected)),
        final_response: None,
        attempts: 1,
        usage_tuple: Some((req.request_id.clone(), 1, harness.target_id)),
    };

    let before = harness.balance();
    harness.pipeline.settle_credit_request(lane, &result).await;
    harness.assert_lane_free().await;

    assert_eq!(
        harness.balance(),
        before - 400,
        "the durable delivered-output checkpoint must be billed on a 499 close"
    );
    let (state, outcome, charged): (String, Option<String>, i64) = harness
        .pool
        .writer()
        .query_row(
            "SELECT state, outcome, charged_units FROM credit_reservations \
             WHERE request_id = ?1 ORDER BY id DESC LIMIT 1",
            [&request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("durable intent row");
    assert_eq!(state, "settled", "a charged partial delivery settles");
    assert_eq!(
        outcome.as_deref(),
        Some("recovered_partial"),
        "the audit trail must name the partial charge"
    );
    assert_eq!(charged, 400);
}
