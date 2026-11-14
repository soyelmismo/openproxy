//! Group-commit checkout: N simultaneous checkouts, ONE fsync.
//!
//! The single-order path ([`super::credits_gateway`]) pays one SQLite commit
//! per order. With `synchronous = FULL` a commit costs one fsync (~2.7 ms on
//! the reference host) while the writer is serialized, so 1500 checkouts
//! arriving together queue for seconds no matter how fast the statements are.
//! This module batches them instead: requests are queued, a committer task
//! drains up to [`CHECKOUT_MAX_BATCH`] of them within [`CHECKOUT_COLLECT_WINDOW`]
//! and writes them in ONE `spawn_write` transaction through
//! [`openproxy_core::credits::orders::create_orders_with_receipts_batch`].
//!
//! Guarantees kept from the single-order path (the batch reuses its body):
//! - every item is receipt-bound and idempotent per receipt;
//! - one item's failure (bad plan, bad window, bad receipt, exhausted jitter)
//!   rolls back only that item (a SAVEPOINT per item) and its caller gets that
//!   error — the neighbours still commit;
//! - a COMMIT failure invalidates the WHOLE batch: no caller receives an
//!   order, because a reply is only sent after the commit returned `Ok`;
//! - a caller that goes away (cancelled request) does not cancel its order and
//!   cannot release anything early: the row is committed with the batch and the
//!   reply is dropped, so a retry with the same receipt replays the same order.
//!
//! Wiring: `#[cfg(feature = "payments")] pub mod checkout_batch;` in
//! `handlers/mod.rs`, one `CheckoutBatcher::new(Arc::clone(state.db_pool()))`
//! stored on `AppState`, and `submit(...)` from the create-order handler in
//! place of `db_pool().spawn_write(create_order_with_receipt)`.

use std::sync::{Arc, OnceLock};

use openproxy_core::credits::orders::{
    BatchOrderRequest, CreateOrderRequest, CreditOrder, create_orders_with_receipts_batch,
};
use openproxy_db::DbPool;
use openproxy_types::{CoreError, Result};
use tokio::sync::{mpsc, oneshot};

/// Queue depth. A bounded channel is the backpressure: a burst deeper than this
/// waits in `submit` instead of growing memory, and the wait is short because a
/// batch drains every few milliseconds.
pub const CHECKOUT_QUEUE_DEPTH: usize = 2048;

/// Largest batch handed to one transaction. Bounded so a single commit stays
/// short (a big batch holds the writer lock and the WAL for longer) and one
/// poisoned item cannot hold up too many callers. 128 halves the writer hold
/// of the worst batch versus 256 at the cost of ~6 extra fsyncs per 1500
/// checkouts — milliseconds against the seconds of queue wait it removes.
pub const CHECKOUT_MAX_BATCH: usize = 128;

/// How long the committer waits for companions before flushing what it has.
/// This is pure added latency per checkout; 2 ms buys batches of tens under load
/// and is invisible next to a checkout round trip.
pub const CHECKOUT_COLLECT_WINDOW: std::time::Duration = std::time::Duration::from_millis(2);

/// One queued checkout plus the channel its answer travels back on.
struct PendingCheckout {
    item: BatchOrderRequest,
    reply: oneshot::Sender<Result<CreditOrder>>,
}

/// Per-`AppState` checkout batcher. Cloning is cheap and shares one queue.
#[derive(Clone)]
pub struct CheckoutBatcher {
    inner: Arc<Inner>,
}

struct Inner {
    pool: Arc<DbPool>,
    /// Created on the first `submit` (or in `new` when a runtime is already
    /// running). `OnceLock` keeps `new` synchronous and race-free.
    sender: OnceLock<mpsc::Sender<PendingCheckout>>,
}

impl std::fmt::Debug for CheckoutBatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CheckoutBatcher")
            .field("started", &self.inner.sender.get().is_some())
            .finish_non_exhaustive()
    }
}

impl CheckoutBatcher {
    /// Build a batcher for `pool`. Starts the committer immediately when a tokio
    /// runtime is current (the normal `AppState` construction path) and defers
    /// to the first [`CheckoutBatcher::submit`] otherwise, so `new` never panics.
    #[must_use]
    pub fn new(pool: Arc<DbPool>) -> Self {
        let batcher = Self {
            inner: Arc::new(Inner {
                pool,
                sender: OnceLock::new(),
            }),
        };
        // Ignore the error: without a runtime the first submit starts the task.
        let _ = batcher.sender();
        batcher
    }

    /// Create one receipt-bound checkout through the group committer.
    ///
    /// Returns only after the batch that contains this item has COMMITTED, so a
    /// returned order is durable (as durable as `synchronous` allows) and
    /// quotable. Dropping this future does NOT cancel the order: the row is
    /// still committed with its batch, and retrying with the same receipt
    /// replays it (that is the whole point of the receipt).
    ///
    /// # Errors
    /// - the item's own validation/DB failure (bad plan, window, receipt, jitter);
    /// - `Internal` when the batcher is not running or the batch COMMIT failed —
    ///   in which case NO order exists for this call.
    pub async fn submit(
        &self,
        request: CreateOrderRequest,
        payment_window_secs: i64,
        receipt: String,
    ) -> Result<CreditOrder> {
        let sender = self.sender()?;
        let (reply, answer) = oneshot::channel();
        let pending = PendingCheckout {
            item: BatchOrderRequest {
                request,
                payment_window_secs,
                receipt,
            },
            reply,
        };
        sender
            .send(pending)
            .await
            .map_err(|_| CoreError::Internal("checkout batcher is not running".into()))?;
        answer.await.map_err(|_| {
            CoreError::Internal(
                "checkout batch ended without a result (the transaction was not committed)".into(),
            )
        })?
    }

    /// Start (once) and return the queue handle.
    fn sender(&self) -> Result<mpsc::Sender<PendingCheckout>> {
        if let Some(sender) = self.inner.sender.get() {
            return Ok(sender.clone());
        }
        let handle = tokio::runtime::Handle::try_current().map_err(|_| {
            CoreError::Internal(
                "checkout batcher needs a tokio runtime to start its committer".into(),
            )
        })?;
        // The task holds only the pool and the receiver: when every batcher
        // handle is dropped the channel closes and the committer exits.
        let pool = Arc::clone(&self.inner.pool);
        let sender = self.inner.sender.get_or_init(|| {
            let (sender, receiver) = mpsc::channel(CHECKOUT_QUEUE_DEPTH);
            // Detached on purpose: the batcher owns the queue, not the handle.
            let _ = handle.spawn(run_committer(pool, receiver));
            sender
        });
        Ok(sender.clone())
    }
}

/// Drain the queue and commit in batches until every sender is gone.
async fn run_committer(pool: Arc<DbPool>, mut queue: mpsc::Receiver<PendingCheckout>) {
    loop {
        // Park until there is work: no polling, no timer while idle.
        let Some(first) = queue.recv().await else {
            return;
        };
        let mut batch = Vec::with_capacity(CHECKOUT_MAX_BATCH);
        batch.push(first);
        let mut drained = false;
        let deadline = tokio::time::Instant::now() + CHECKOUT_COLLECT_WINDOW;
        while batch.len() < CHECKOUT_MAX_BATCH {
            match tokio::time::timeout_at(deadline, queue.recv()).await {
                Ok(Some(next)) => batch.push(next),
                Ok(None) => {
                    // Batcher dropped mid-collect: flush this batch, then stop.
                    drained = true;
                    break;
                }
                Err(_elapsed) => break,
            }
        }
        commit_batch(&pool, batch).await;
        if drained {
            return;
        }
    }
}

/// One writer transaction for the whole batch, then one reply per item.
async fn commit_batch(pool: &Arc<DbPool>, batch: Vec<PendingCheckout>) {
    let (items, replies): (
        Vec<BatchOrderRequest>,
        Vec<oneshot::Sender<Result<CreditOrder>>>,
    ) = batch
        .into_iter()
        .map(|pending| (pending.item, pending.reply))
        .unzip();
    let written = pool
        .spawn_write(move |writer| create_orders_with_receipts_batch(writer, &items))
        .await;
    match written {
        Ok(results) => {
            for (reply, result) in replies.into_iter().zip(results) {
                // A closed receiver means the caller went away; the order is
                // already committed and a receipt retry replays it.
                let _ = reply.send(result);
            }
        }
        Err(e) => {
            // The transaction did not land: no item of this batch exists, so
            // every waiter gets an error and nobody may quote an order.
            let message = format!("checkout batch commit failed: {e}");
            for reply in replies {
                let _ = reply.send(Err(CoreError::Internal(message.clone())));
            }
        }
    }
}
