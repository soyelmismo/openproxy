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

/// Generic group-commit queue shared by [`CheckoutBatcher`] and
/// [`super::delivery_batch::DeliveryBatcher`].
///
///Lazily spawns a detached committer task on the current Tokio runtime and
///collects up to `max_batch` items within `collect_window` before invoking
///`commit_fn` in a single batch.
pub(crate) struct GroupCommitQueue<T> {
    sender: OnceLock<mpsc::Sender<T>>,
    queue_depth: usize,
    max_batch: usize,
    collect_window: std::time::Duration,
    not_running_msg: &'static str,
    no_runtime_msg: &'static str,
}

impl<T: Send + 'static> GroupCommitQueue<T> {
    pub(crate) const fn new(
        queue_depth: usize,
        max_batch: usize,
        collect_window: std::time::Duration,
        not_running_msg: &'static str,
        no_runtime_msg: &'static str,
    ) -> Self {
        Self {
            sender: OnceLock::new(),
            queue_depth,
            max_batch,
            collect_window,
            not_running_msg,
            no_runtime_msg,
        }
    }

    pub(crate) fn is_started(&self) -> bool {
        self.sender.get().is_some()
    }

    pub(crate) fn sender_or_spawn<F, Fut>(&self, commit_fn: F) -> Result<mpsc::Sender<T>>
    where
        F: Fn(Vec<T>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        if let Some(sender) = self.sender.get() {
            return Ok(sender.clone());
        }
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| CoreError::Internal(self.no_runtime_msg.into()))?;
        let queue_depth = self.queue_depth;
        let max_batch = self.max_batch;
        let collect_window = self.collect_window;
        let sender = self.sender.get_or_init(|| {
            let (sender, mut receiver) = mpsc::channel::<T>(queue_depth);
            let _ = handle.spawn(async move {
                loop {
                    let Some(first) = receiver.recv().await else {
                        return;
                    };
                    let mut batch = Vec::with_capacity(max_batch);
                    batch.push(first);
                    let mut drained = false;
                    let deadline = tokio::time::Instant::now() + collect_window;
                    while batch.len() < max_batch {
                        match tokio::time::timeout_at(deadline, receiver.recv()).await {
                            Ok(Some(next)) => batch.push(next),
                            Ok(None) => {
                                drained = true;
                                break;
                            }
                            Err(_elapsed) => break,
                        }
                    }
                    commit_fn(batch).await;
                    if drained {
                        return;
                    }
                }
            });
            sender
        });
        Ok(sender.clone())
    }

    pub(crate) async fn send<F, Fut>(&self, item: T, commit_fn: F) -> Result<()>
    where
        F: Fn(Vec<T>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        self.sender_or_spawn(commit_fn)?
            .send(item)
            .await
            .map_err(|_| CoreError::Internal(self.not_running_msg.into()))
    }
}

/// Per-`AppState` checkout batcher. Cloning is cheap and shares one queue.
#[derive(Clone)]
pub struct CheckoutBatcher {
    inner: Arc<Inner>,
}

struct Inner {
    pool: Arc<DbPool>,
    queue: GroupCommitQueue<PendingCheckout>,
}

impl std::fmt::Debug for CheckoutBatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CheckoutBatcher")
            .field("started", &self.inner.queue.is_started())
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
                queue: GroupCommitQueue::new(
                    CHECKOUT_QUEUE_DEPTH,
                    CHECKOUT_MAX_BATCH,
                    CHECKOUT_COLLECT_WINDOW,
                    "checkout batcher is not running",
                    "checkout batcher needs a tokio runtime to start its committer",
                ),
            }),
        };
        let _ = batcher.sender();
        batcher
    }

    /// Create one receipt-bound checkout through the group committer.
    pub async fn submit(
        &self,
        request: CreateOrderRequest,
        payment_window_secs: i64,
        receipt: String,
    ) -> Result<CreditOrder> {
        let (reply, answer) = oneshot::channel();
        let pending = PendingCheckout {
            item: BatchOrderRequest {
                request,
                payment_window_secs,
                receipt,
            },
            reply,
        };
        let pool = Arc::clone(&self.inner.pool);
        self.inner
            .queue
            .send(pending, move |batch| {
                let pool = Arc::clone(&pool);
                async move { commit_batch(&pool, batch).await }
            })
            .await?;
        answer.await.map_err(|_| {
            CoreError::Internal(
                "checkout batch ended without a result (the transaction was not committed)".into(),
            )
        })?
    }

    fn sender(&self) -> Result<mpsc::Sender<PendingCheckout>> {
        let pool = Arc::clone(&self.inner.pool);
        self.inner.queue.sender_or_spawn(move |batch| {
            let pool = Arc::clone(&pool);
            async move { commit_batch(&pool, batch).await }
        })
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
                let _ = reply.send(result);
            }
        }
        Err(e) => {
            let message = format!("checkout batch commit failed: {e}");
            for reply in replies {
                let _ = reply.send(Err(CoreError::Internal(message.clone())));
            }
        }
    }
}
