//! Group-commit delivery: N simultaneous reveals/acks, ONE fsync.
//!
//! The single-order path ([`super::credits_gateway`]) pays one SQLite commit
//! per reveal and per ack. With `synchronous = FULL` a commit costs one fsync
//! (~2.7 ms on the reference host) while the writer is serialized, so 1500
//! reveals + 1500 acks arriving together queue for seconds. This module
//! batches them: requests are queued, a committer task drains up to
//! [`DELIVERY_MAX_BATCH`] of them within [`DELIVERY_COLLECT_WINDOW`] and
//! writes them in ONE `spawn_write` transaction through
//! [`openproxy_core::credits::delivery::apply_delivery_batch`].
//!
//! Guarantees kept from the single-order path (the batch reuses its body):
//! - every item is receipt-bound and idempotent per receipt (a repeated reveal
//!   re-reads the stored ciphertext; a repeated ack is a no-op success);
//! - one item's failure (unknown receipt, decrypt failure) rolls back only
//!   that item (a SAVEPOINT per item) and its caller gets that error — the
//!   neighbours still commit;
//! - a COMMIT failure invalidates the WHOLE batch: no caller receives a
//!   plaintext or an ack success, because a reply is only sent after the
//!   commit returned `Ok`. A reveal reply after a failed commit is impossible,
//!   and a retry simply re-reads the still-stored ciphertext;
//! - a caller that goes away (cancelled request) does not undo its item: the
//!   reveal mark (or the wipe) is committed with the batch and the reply is
//!   dropped, which the receipt contract already treats as replayable.

use std::sync::Arc;

use openproxy_core::credits::{DeliveryOp, DeliveryOutcome, apply_delivery_batch};
use openproxy_db::{DbPool, MasterKey};
use openproxy_types::{CoreError, Result};
use tokio::sync::{mpsc, oneshot};

use super::checkout_batch::GroupCommitQueue;

/// Queue depth. A bounded channel is the backpressure: a burst deeper than
/// this waits in `submit_*` instead of growing memory.
pub const DELIVERY_QUEUE_DEPTH: usize = 2048;

/// Largest batch handed to one transaction. Bounded so a single commit stays
/// short (a big batch holds the writer lock and the WAL for longer) and one
/// poisoned item cannot hold up too many callers.
pub const DELIVERY_MAX_BATCH: usize = 128;

/// How long the committer waits for companions before flushing what it has.
/// Pure added latency per item; 2 ms buys batches of tens under load and is
/// invisible next to a checkout round trip.
pub const DELIVERY_COLLECT_WINDOW: std::time::Duration = std::time::Duration::from_millis(2);

/// One queued delivery item plus the channel its answer travels back on.
struct PendingDelivery {
    code: String,
    op: DeliveryOp,
    reply: oneshot::Sender<DeliveryReply>,
}

/// Reply payload before endpoint-specific unwrapping.
enum DeliveryReply {
    Revealed(Result<Option<String>>),
    Acked(Result<bool>),
}

/// Per-`AppState` delivery batcher. Cloning is cheap and shares one queue.
#[derive(Clone)]
pub struct DeliveryBatcher {
    inner: Arc<Inner>,
}

struct Inner {
    pool: Arc<DbPool>,
    /// The master key decrypts minted keys; fixed for the process lifetime.
    master: Arc<MasterKey>,
    queue: GroupCommitQueue<PendingDelivery>,
}

impl std::fmt::Debug for DeliveryBatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeliveryBatcher")
            .field("started", &self.inner.queue.is_started())
            .finish_non_exhaustive()
    }
}

impl DeliveryBatcher {
    /// Build a batcher for `pool`. Starts the committer immediately when a
    /// tokio runtime is current (the normal `AppState` construction path) and
    /// defers to the first submit otherwise, so `new` never panics.
    #[must_use]
    pub fn new(pool: Arc<DbPool>, master: Arc<MasterKey>) -> Self {
        let batcher = Self {
            inner: Arc::new(Inner {
                pool,
                master,
                queue: GroupCommitQueue::new(
                    DELIVERY_QUEUE_DEPTH,
                    DELIVERY_MAX_BATCH,
                    DELIVERY_COLLECT_WINDOW,
                    "delivery batcher is not running",
                    "delivery batcher needs a tokio runtime to start its committer",
                ),
            }),
        };
        let _ = batcher.sender();
        batcher
    }

    /// Retry the delivery of a minted key through the group committer.
    pub async fn submit_reveal(&self, code: String, receipt: String) -> Result<Option<String>> {
        match self.submit(code, DeliveryOp::Reveal { receipt }).await? {
            DeliveryReply::Revealed(r) => r,
            DeliveryReply::Acked(_) => Err(CoreError::Internal(
                "delivery batch returned an ack for a reveal".into(),
            )),
        }
    }

    /// Acknowledge a delivery and wipe the ciphertext through the group
    /// committer. Reply only after the batch COMMITTED.
    pub async fn submit_ack(&self, code: String, receipt: String) -> Result<bool> {
        match self.submit(code, DeliveryOp::Ack { receipt }).await? {
            DeliveryReply::Acked(r) => r,
            DeliveryReply::Revealed(_) => Err(CoreError::Internal(
                "delivery batch returned a reveal for an ack".into(),
            )),
        }
    }

    /// Queue one item and wait for its batch to commit.
    async fn submit(&self, code: String, op: DeliveryOp) -> Result<DeliveryReply> {
        let (reply, answer) = oneshot::channel::<DeliveryReply>();
        let pending = PendingDelivery { code, op, reply };
        let pool = Arc::clone(&self.inner.pool);
        let master = Arc::clone(&self.inner.master);
        self.inner
            .queue
            .send(pending, move |batch| {
                let pool = Arc::clone(&pool);
                let master = Arc::clone(&master);
                async move { commit_batch(&pool, &master, batch).await }
            })
            .await?;
        answer.await.map_err(|_| {
            CoreError::Internal(
                "delivery batch ended without a result (the transaction was not committed)".into(),
            )
        })
    }

    fn sender(&self) -> Result<mpsc::Sender<PendingDelivery>> {
        let pool = Arc::clone(&self.inner.pool);
        let master = Arc::clone(&self.inner.master);
        self.inner.queue.sender_or_spawn(move |batch| {
            let pool = Arc::clone(&pool);
            let master = Arc::clone(&master);
            async move { commit_batch(&pool, &master, batch).await }
        })
    }
}

/// One writer transaction for the whole batch, then one reply per item.
async fn commit_batch(pool: &Arc<DbPool>, master: &Arc<MasterKey>, batch: Vec<PendingDelivery>) {
    let written = {
        let master = Arc::clone(master);
        let items: Vec<(String, DeliveryOp)> = batch
            .iter()
            .map(|pending| (pending.code.clone(), pending.op.clone()))
            .collect();
        pool.spawn_write(move |writer| apply_delivery_batch(writer, &master, &items))
            .await
    };
    match written {
        Ok(results) => {
            for (pending, result) in batch.into_iter().zip(results) {
                let reply = match result {
                    Ok(DeliveryOutcome::Revealed(plaintext)) => {
                        DeliveryReply::Revealed(Ok(plaintext))
                    }
                    Ok(DeliveryOutcome::Acked(acked)) => DeliveryReply::Acked(Ok(acked)),
                    Err(e) => match pending.op {
                        DeliveryOp::Reveal { .. } => DeliveryReply::Revealed(Err(e)),
                        DeliveryOp::Ack { .. } => DeliveryReply::Acked(Err(e)),
                    },
                };
                let _ = pending.reply.send(reply);
            }
        }
        Err(e) => {
            let message = format!("delivery batch commit failed: {e}");
            for pending in batch {
                let reply = match pending.op {
                    DeliveryOp::Reveal { .. } => {
                        DeliveryReply::Revealed(Err(CoreError::Internal(message.clone())))
                    }
                    DeliveryOp::Ack { .. } => {
                        DeliveryReply::Acked(Err(CoreError::Internal(message.clone())))
                    }
                };
                let _ = pending.reply.send(reply);
            }
        }
    }
}
