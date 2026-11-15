use super::{
    ApiKeyId, CoreError, CreditsAdmissionGate, CreditsLaneGuard, DbPool, PendingUnaryUsage,
    RequestId, UnaryPersistError, UsageInput, admit_prepaid, persist_unary_usage, reservation,
};

/// Admit one **non-chat** request (audio, embeddings, images, systemone) under
/// the same process-wide per-key lane and fail-closed policy chat uses.
///
/// Call it before touching the upstream and keep the returned guard until
/// [`settle_unary_credit`] has persisted the usage row; dropping the guard
/// without settling releases the lane and bills nothing, which is correct for
/// a request that never produced a response.
pub async fn admit_unary_credit(
    db_pool: &DbPool,
    api_key_id: Option<ApiKeyId>,
) -> openproxy_types::Result<Option<CreditsLaneGuard>> {
    admit_unary_credit_with_request(db_pool, api_key_id, RequestId::new(), None).await
}

/// Same, with the request id the usage row will carry and an optional
/// maximum-cost quote.
///
/// Passing the real [`RequestId`] matters after a crash: the recovering
/// admission looks the durable intent up against the `usage` table by that id
/// and can bill the response that was actually delivered instead of releasing
/// the intent. Without it the intent is still resolved (never left open), just
/// without usage evidence to reconcile.
pub async fn admit_unary_credit_with_request(
    db_pool: &DbPool,
    api_key_id: Option<ApiKeyId>,
    request_id: RequestId,
    budget_units: Option<i64>,
) -> openproxy_types::Result<Option<CreditsLaneGuard>> {
    admit_prepaid(db_pool, api_key_id, request_id.to_string(), 1, budget_units).await
}

/// Persist a unary usage row (whose insert applies its ledger debit) and only
/// then release the key lane.
///
/// The error is deliberately returned rather than logged and dropped: a caller
/// that cannot prove the charge was recorded must not answer as if it had
/// been, or the next request would spend against a balance that misses this
/// debit. When the journal append itself failed nothing durable exists, so the
/// row is retained as a pending marker and the following admission for that key
/// re-appends it (and is denied until that works).
pub async fn settle_unary_credit(
    db_pool: &DbPool,
    lane: Option<CreditsLaneGuard>,
    usage: UsageInput,
) -> openproxy_types::Result<()> {
    let gate = CreditsAdmissionGate::new();
    let marker = PendingUnaryUsage::new(usage.clone());
    let conn = db_pool.writer_arc();
    let persisted = tokio::task::spawn_blocking(move || persist_unary_usage(&conn, &usage)).await;

    let failure = match persisted {
        Ok(Ok(())) => {
            reservation::close_unary(db_pool, lane.as_ref(), true).await;
            drop(lane);
            return Ok(());
        }
        Ok(Err(UnaryPersistError::Journaled(error))) => {
            // A durable copy exists and the next drain applies it exactly once,
            // so the lane can go and no marker is needed.
            reservation::close_unary(db_pool, lane.as_ref(), true).await;
            tracing::warn!(
                %error,
                "credit settlement: unary usage not applied yet; left journaled"
            );
            error
        }
        Ok(Err(UnaryPersistError::Lost(error))) => {
            // Nothing was written anywhere: keep the charge so the next
            // admission re-appends it instead of trusting a balance that
            // misses this debit.
            match lane.as_ref() {
                Some(lane) => {
                    tracing::error!(
                        %error,
                        api_key_id = lane.key_id(),
                        "credit settlement: unary usage lost; keeping the key closed"
                    );
                    gate.note_pending_unary_usage(lane.db_id(), lane.key_id(), marker);
                }
                None => {
                    tracing::error!(%error, "credit settlement: unary usage lost");
                }
            }
            error
        }
        Err(error) => {
            // The blocking task never reported: treat it as lost. The retry
            // de-duplicates by request id, so an append that did commit cannot
            // become a second row.
            if let Some(lane) = lane.as_ref() {
                gate.note_pending_unary_usage(lane.db_id(), lane.key_id(), marker);
            }
            CoreError::Internal(format!(
                "unary usage persistence task failed to run: {error}"
            ))
        }
    };
    // Releasing after the marker is set is what makes the next admission see
    // the debt; the lock itself is never the safety net.
    drop(lane);
    Err(failure)
}
