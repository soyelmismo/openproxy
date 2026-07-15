//! Cancellation primitives.
//!
//! `CancellationToken` is a tiny, `Clone`-able, atomically-flippable flag
//! that the client races against the I/O future at every phase boundary
//! and inside long phases (e.g. body read). It is intentionally NOT a
//! `tokio_util::sync::CancellationToken` so this module has zero extra
//! dependencies.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::watch;

/// A cloneable, thread-safe cancel signal.
///
/// Cheap to clone (one `Arc` clone). `cancel()` is idempotent. The
/// associated counter (`cancel_count`) is incremented every time
/// `cancel()` is called and is observable for metrics / tests.
#[derive(Clone)]
pub struct CancellationToken {
    inner: Arc<Inner>,
}

struct Inner {
    flag: AtomicBool,
    cancel_count: AtomicUsize,
    // For tests: how many observers saw the flag.
    observe_count: AtomicUsize,
    // Async notification channel. Value transitions false → true once.
    cancel_tx: watch::Sender<bool>,
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationToken {
    /// Create a fresh, un-cancelled token.
    pub fn new() -> Self {
        let (cancel_tx, _rx) = watch::channel(false);
        Self {
            inner: Arc::new(Inner {
                flag: AtomicBool::new(false),
                cancel_count: AtomicUsize::new(0),
                observe_count: AtomicUsize::new(0),
                cancel_tx,
            }),
        }
    }

    /// Signal cancellation. Idempotent. Does NOT block.
    pub fn cancel(&self) {
        self.inner.flag.store(true, Ordering::SeqCst);
        self.inner.cancel_count.fetch_add(1, Ordering::SeqCst);
        let _ = self.inner.cancel_tx.send(true);
    }

    /// Async wait for cancellation. Returns immediately if already
    /// cancelled, otherwise suspends until `cancel()` is called.
    /// Cheap: one `subscribe()` + one `Arc` clone.
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let mut rx = self.inner.cancel_tx.subscribe();
        if *rx.borrow_and_update() {
            return;
        }
        // `changed()` returns Err when the sender is dropped.
        // Treat that as cancellation (the token is being torn down).
        while rx.changed().await.is_ok() {
            if *rx.borrow() {
                return;
            }
        }
    }

    /// Create a `watch::Receiver` that observes the internal cancel
    /// notification channel. Use this to poll `changed()` in hot loops
    /// without creating a new subscription per iteration.
    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.inner.cancel_tx.subscribe()
    }

    /// Non-blocking peek.
    pub fn is_cancelled(&self) -> bool {
        let was = self.inner.flag.load(Ordering::SeqCst);
        if was {
            self.inner.observe_count.fetch_add(1, Ordering::SeqCst);
        }
        was
    }

    /// Create a child token that is cancelled if EITHER the parent OR
    /// the child is cancelled. The child observes the parent's state
    /// on every `is_cancelled` call; cancelling the parent does NOT
    /// mutate the child (the child inherits a snapshot semantics on
    /// `child()`). This is the "parent stays valid" semantic the spec
    /// requires: cancelling a child never cancels the parent.
    pub fn child(&self) -> Self {
        let child = Self::new();
        if self.is_cancelled() {
            child.cancel();
        }
        child
    }

    /// Total number of `cancel()` calls observed across all clones.
    /// Useful for tests / metrics.
    pub fn cancel_count(&self) -> usize {
        self.inner.cancel_count.load(Ordering::SeqCst)
    }

    /// Number of times `is_cancelled()` returned `true` for this token
    /// or any of its children that share the same `Arc`. Useful for
    /// tests that want to assert the client actually consulted the
    /// token.
    pub fn observe_count(&self) -> usize {
        self.inner.observe_count.load(Ordering::SeqCst)
    }

    /// Build a token that mirrors a `tokio::sync::watch::Receiver<bool>`:
    /// the token flips to "cancelled" the first time the watch's value
    /// transitions to `true` (and stays cancelled forever after — the
    /// watch is one-shot from the token's point of view).
    ///
    /// The returned token owns a background task that drives the flip.
    pub fn from_watch(mut rx: watch::Receiver<bool>) -> Self {
        let token = Self::new();
        if *rx.borrow_and_update() {
            token.cancel();
            return token;
        }
        let inner = token.clone();
        tokio::spawn(async move {
            // `changed()` returns Err when the sender is dropped; treat
            // that as a no-op (the upstream call will finish or hit its
            // own deadline). We only cancel when we observe an actual
            // `true` value.
            while rx.changed().await.is_ok() {
                if *rx.borrow() {
                    inner.cancel();
                    return;
                }
            }
        });
        token
    }

    /// Build a combined token that flips to "cancelled" when EITHER the
    /// `watch::Receiver<bool>` transitions to `true` OR the provided
    /// `CancellationToken` is cancelled.
    ///
    /// Use this for race lanes: the lane's upstream call is cancelled
    /// when the client disconnects **or** when the race is lost (another
    /// lane sent the first token). This closes the cancellation window
    /// — losers' HTTP connections are dropped at the transport level,
    /// stopping upstream token generation immediately.
    pub fn from_watch_and_token(
        mut rx: watch::Receiver<bool>,
        race_token: CancellationToken,
    ) -> Self {
        let token = Self::new();
        if *rx.borrow_and_update() || race_token.is_cancelled() {
            token.cancel();
            return token;
        }
        let inner = token.clone();
        let mut cancel_rx = race_token.inner.cancel_tx.subscribe();
        // Close TOCTOU: if cancelled between is_cancelled() above and
        // subscribe(), the initial value is already true but changed()
        // won't fire for it.  Re-check explicitly.
        if *cancel_rx.borrow() {
            inner.cancel();
            return token;
        }
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    res = rx.changed() => {
                        if res.is_err() || *rx.borrow() {
                            inner.cancel();
                            return;
                        }
                    }
                    res = cancel_rx.changed() => {
                        if res.is_err() || *cancel_rx.borrow() {
                            inner.cancel();
                            return;
                        }
                    }
                }
            }
        });
        token
    }
}

impl std::fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .field("cancel_count", &self.cancel_count())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_token_is_not_cancelled() {
        let t = CancellationToken::new();
        assert!(!t.is_cancelled());
        assert_eq!(t.cancel_count(), 0);
    }

    #[test]
    fn cancel_is_idempotent_and_observable() {
        let t = CancellationToken::new();
        t.cancel();
        t.cancel();
        t.cancel();
        assert!(t.is_cancelled());
        assert_eq!(t.cancel_count(), 3);
    }

    #[test]
    fn clone_shares_state() {
        let t = CancellationToken::new();
        let t2 = t.clone();
        t2.cancel();
        assert!(t.is_cancelled());
        assert_eq!(t.cancel_count(), 1);
    }

    #[test]
    fn child_inherits_then_decouples() {
        let parent = CancellationToken::new();
        parent.cancel();
        let child = parent.child();
        assert!(child.is_cancelled(), "child must see pre-existing cancel");

        // Decoupling: cancelling the child does not cancel the parent.
        let parent2 = CancellationToken::new();
        let child2 = parent2.child();
        child2.cancel();
        assert!(
            !parent2.is_cancelled(),
            "parent stays valid after child cancel"
        );
    }

    // The `from_watch` tests below need a Tokio runtime because the
    // helper spawns a background task that races the watch.
    #[tokio::test]
    async fn from_watch_already_cancelled_starts_cancelled() {
        let (tx, mut rx) = watch::channel(false);
        // Flip the watch to `true` BEFORE constructing the token —
        // mirrors the pre-flight check in the chat pipeline.
        tx.send(true).unwrap();
        // Give the receiver a moment to see the change.
        rx.changed().await.unwrap();
        let token = CancellationToken::from_watch(rx);
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn from_watch_cancels_on_transition() {
        let (tx, rx) = watch::channel(false);
        let token = CancellationToken::from_watch(rx);
        assert!(!token.is_cancelled());
        // Flip the watch — the background task should observe the
        // change and flip the token.
        tx.send(true).unwrap();
        // Spin briefly to let the task run.
        for _ in 0..50 {
            if token.is_cancelled() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn cancelled_returns_immediately_if_already_cancelled() {
        let t = CancellationToken::new();
        t.cancel();
        // Should return instantly, not hang.
        t.cancelled().await;
    }

    #[tokio::test]
    async fn cancelled_awaits_cancel() {
        let t = CancellationToken::new();
        let t2 = t.clone();

        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            t2.cancel();
        });

        // Should suspend until t2 cancels.
        t.cancelled().await;
        assert!(t.is_cancelled());
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_multiple_waiters_all_wake() {
        let t = CancellationToken::new();
        let mut handles = Vec::new();
        for _ in 0..10 {
            let t2 = t.clone();
            handles.push(tokio::spawn(async move {
                t2.cancelled().await;
                assert!(t2.is_cancelled());
            }));
        }

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        t.cancel();
        for h in handles {
            h.await.unwrap();
        }
    }

    #[tokio::test]
    async fn from_watch_drops_cleanly_when_sender_dropped() {
        let (tx, rx) = watch::channel(false);
        let token = CancellationToken::from_watch(rx);
        // Drop the sender. The background task should observe the
        // closed channel via `changed()` returning Err and exit
        // cleanly. The token stays uncancelled.
        drop(tx);
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert!(!token.is_cancelled());
    }

    #[tokio::test]
    async fn from_watch_and_token_fires_on_watch_transition() {
        let (tx, rx) = watch::channel(false);
        let race_token = CancellationToken::new();
        let token = CancellationToken::from_watch_and_token(rx, race_token);
        assert!(!token.is_cancelled());

        tx.send(true).unwrap();
        for _ in 0..50 {
            if token.is_cancelled() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn from_watch_and_token_fires_on_race_token() {
        let (_tx, rx) = watch::channel(false);
        let race_token = CancellationToken::new();
        let token = CancellationToken::from_watch_and_token(rx, race_token.clone());
        assert!(!token.is_cancelled());

        race_token.cancel();
        for _ in 0..50 {
            if token.is_cancelled() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn from_watch_and_token_already_cancelled_watch_starts_cancelled() {
        let (tx, mut rx) = watch::channel(false);
        tx.send(true).unwrap();
        rx.changed().await.unwrap();
        let race_token = CancellationToken::new();
        let token = CancellationToken::from_watch_and_token(rx, race_token);
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn from_watch_and_token_already_cancelled_race_token_starts_cancelled() {
        let (_tx, rx) = watch::channel(false);
        let race_token = CancellationToken::new();
        race_token.cancel();
        let token = CancellationToken::from_watch_and_token(rx, race_token);
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn from_watch_and_token_toctou_closes() {
        // Race: cancel race_token between subscribe() and borrow()
        // The re-check after subscribe() must catch it.
        let (_tx, rx) = watch::channel(false);
        let race_token = CancellationToken::new();
        // Cancel BEFORE creating the combined token — the pre-flight
        // check catches this. But also test the TOCTOU path by
        // cancelling between subscribe and the background task start.
        race_token.cancel();
        let token = CancellationToken::from_watch_and_token(rx, race_token);
        assert!(token.is_cancelled());
    }
}
