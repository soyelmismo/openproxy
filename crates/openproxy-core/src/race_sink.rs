//! Race-aware stream sink for multi-lane race mode.
//!
//! When `combo.race_size > 1`, multiple workers race to produce the first
//! token. `RaceSink` determines the winner at the **chunk level**: the
//! first worker to call `send()` wins. All other workers' tokens are
//! instantly cancelled at the HTTP-transport level (the upstream
//! `CancellationToken` fires), and their chunks are silently discarded.
//!
//! This eliminates the interleaving window that existed in the old
//! forwarding-task architecture: the old code declared the winner on
//! *stream completion*, not on first token, and buffered chunks from
//! losing workers could leak through the forwarding tasks before the
//! 10-ms polling loop detected the winner and fired `race_cancel`.

use crate::upstream::CancellationToken;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::mpsc;

/// Errors that can occur when writing to a [`StreamSink`].
#[derive(Debug, thiserror::Error)]
pub enum StreamSinkError {
    /// The underlying channel to the client was closed.
    #[error("stream sink closed")]
    Closed,
    /// This lane lost the race; its chunks must be discarded.
    #[error("race lost")]
    Lost,
}

// ---------------------------------------------------------------------------
// StreamSink enum
// ---------------------------------------------------------------------------

/// Unified stream sink used by `dispatch_upstream_streaming`.
///
/// In non-race mode this wraps the original `mpsc::Sender` directly
/// (zero overhead). In race mode it wraps a [`RaceSinkHandle`] that
/// races against other lanes to claim the first-token winner slot.
#[derive(Debug, Clone)]
pub enum StreamSink {
    Direct(mpsc::Sender<bytes::Bytes>),
    Race(RaceSinkHandle),
    /// Discard all chunks. Used for non-streaming client requests where
    /// the pipeline still uses the streaming path to the upstream (to
    /// get proper TTFT + timeout semantics) but the client doesn't want
    /// SSE — the pipeline accumulates the response internally and
    /// returns it as a single JSON object.
    Discard,
}

impl StreamSink {
    /// Forward a chunk to the client (or attempt to claim the winner slot
    /// in race mode).
    pub async fn send(&self, chunk: bytes::Bytes) -> Result<(), StreamSinkError> {
        match self {
            StreamSink::Direct(tx) => tx.send(chunk).await.map_err(|_| StreamSinkError::Closed),
            StreamSink::Race(handle) => handle.send(chunk).await,
            StreamSink::Discard => Ok(()), // silently drop
        }
    }
}

// ---------------------------------------------------------------------------
// RaceSink + RaceSinkHandle
// ---------------------------------------------------------------------------

/// Shared atomic arbiter for a single race round.
///
/// Created once per `run_race` invocation; each worker gets a
/// [`RaceSinkHandle`] via [`handle()`](RaceSink::handle).
pub struct RaceSink {
    /// The original client-facing channel.
    inner: mpsc::Sender<bytes::Bytes>,
    /// 0 = undecided; otherwise `worker_id + 1`.
    winner: AtomicUsize,
    /// Per-worker cancellation tokens.  When the first chunk arrives
    /// the `RaceSink` cancels all tokens except the winner's.
    worker_tokens: Vec<CancellationToken>,
}

impl std::fmt::Debug for RaceSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RaceSink")
            .field("winner", &self.winner.load(Ordering::Relaxed))
            .field("workers", &self.worker_tokens.len())
            .finish()
    }
}

impl RaceSink {
    /// Create a new race sink backed by the given client channel.
    ///
    /// Returns the shared sink (behind `Arc`) and one
    /// [`CancellationToken`] per worker.  Each worker's upstream call
    /// must use its token (combined with the client disconnect watch)
    /// so that losing the race cancels the HTTP request at the
    /// transport level.
    pub fn new(
        inner: mpsc::Sender<bytes::Bytes>,
        num_workers: usize,
    ) -> (Arc<Self>, Vec<CancellationToken>) {
        let worker_tokens: Vec<CancellationToken> =
            (0..num_workers).map(|_| CancellationToken::new()).collect();
        let sink = Arc::new(Self {
            inner,
            winner: AtomicUsize::new(0),
            worker_tokens: worker_tokens.clone(),
        });
        (sink, worker_tokens)
    }

    /// Create a handle for a specific worker.  The handle is cheap to
    /// clone and can be sent across tasks.
    pub fn handle(self: &Arc<Self>, worker_id: usize) -> RaceSinkHandle {
        RaceSinkHandle {
            sink: Arc::clone(self),
            worker_id,
        }
    }

    /// Core send path.  The first caller to invoke `send` with any
    /// `worker_id` wins: the `race_cancel` tokens for all other
    /// workers are cancelled, and the chunk is forwarded to the
    /// client.  Subsequent calls from losing workers return
    /// [`StreamSinkError::Lost`] synchronously (zero await).
    async fn send(&self, worker_id: usize, chunk: bytes::Bytes) -> Result<(), StreamSinkError> {
        // Fast path: already decided and it's not us.
        let current = self.winner.load(Ordering::Acquire);
        if current != 0 && current != worker_id + 1 {
            return Err(StreamSinkError::Lost);
        }

        if current == 0 {
            // Attempt to atomically claim the winner slot.
            match self.winner.compare_exchange(
                0,
                worker_id + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // We won.  Cancel all other workers' tokens
                    // *before* forwarding to the client so that the
                    // losers' upstream TCP connections are RST'd
                    // immediately — no extra token generation.
                    for (idx, token) in self.worker_tokens.iter().enumerate() {
                        if idx != worker_id {
                            token.cancel();
                        }
                    }
                }
                Err(existing) => {
                    if existing != worker_id + 1 {
                        return Err(StreamSinkError::Lost);
                    }
                    // existing == worker_id + 1: we're the winner,
                    // race was already decided by an earlier send
                    // from us.  Fall through to forward.
                }
            }
        }

        self.inner
            .send(chunk)
            .await
            .map_err(|_| StreamSinkError::Closed)
    }
}

/// A per-worker handle into a shared [`RaceSink`].
///
/// Each worker clones the handle and holds it for the duration of its
/// `dispatch_upstream_streaming` call.  The `worker_id` identifies the
/// lane; the first lane to call `send` on any handle into the same
/// `RaceSink` wins.
#[derive(Debug, Clone)]
pub struct RaceSinkHandle {
    sink: Arc<RaceSink>,
    worker_id: usize,
}

impl RaceSinkHandle {
    /// Forward a chunk through the race sink.  Returns `Ok(())` if
    /// this worker is the (or a) winner, or `Err(Lost)` if another
    /// worker already claimed the race.
    pub async fn send(&self, chunk: bytes::Bytes) -> Result<(), StreamSinkError> {
        self.sink.send(self.worker_id, chunk).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn first_sender_wins() {
        let (tx, mut rx) = mpsc::channel(16);
        let (sink, tokens) = RaceSink::new(tx, 2);
        let h0 = sink.handle(0);
        let h1 = sink.handle(1);

        // Worker 0 sends first — should win
        h0.send(bytes::Bytes::from("chunk0")).await.unwrap();

        // Worker 1 should get Lost
        assert!(h1.send(bytes::Bytes::from("chunk1")).await.is_err());

        // Worker 0's token should NOT be cancelled
        assert!(!tokens[0].is_cancelled());
        // Worker 1's token SHOULD be cancelled
        assert!(tokens[1].is_cancelled());

        // Client should receive the winner's chunk
        let chunk = rx.recv().await.unwrap();
        assert_eq!(chunk.as_ref(), b"chunk0");
    }

    #[tokio::test]
    async fn winner_can_send_multiple_chunks() {
        let (tx, mut rx) = mpsc::channel(16);
        let (sink, tokens) = RaceSink::new(tx, 2);
        let h0 = sink.handle(0);
        let h1 = sink.handle(1);

        h0.send(bytes::Bytes::from("a")).await.unwrap();
        h0.send(bytes::Bytes::from("b")).await.unwrap();
        h0.send(bytes::Bytes::from("c")).await.unwrap();

        assert!(h1.send(bytes::Bytes::from("x")).await.is_err());

        assert_eq!(rx.recv().await.unwrap().as_ref(), b"a");
        assert_eq!(rx.recv().await.unwrap().as_ref(), b"b");
        assert_eq!(rx.recv().await.unwrap().as_ref(), b"c");
        assert!(!tokens[0].is_cancelled());
        assert!(tokens[1].is_cancelled());
    }

    #[tokio::test]
    async fn concurrent_first_send_only_one_wins() {
        let (tx, mut rx) = mpsc::channel(16);
        let (sink, tokens) = RaceSink::new(tx, 3);
        let h0 = sink.handle(0);
        let h1 = sink.handle(1);
        let h2 = sink.handle(2);

        // Race: all three send concurrently
        let (r0, r1, r2) = tokio::join!(
            h0.send(bytes::Bytes::from("0")),
            h1.send(bytes::Bytes::from("1")),
            h2.send(bytes::Bytes::from("2")),
        );

        // Exactly one should succeed
        let wins = [r0.is_ok(), r1.is_ok(), r2.is_ok()];
        assert_eq!(wins.iter().filter(|&&b| b).count(), 1, "exactly one winner");

        // Exactly two losers should have their tokens cancelled
        let cancelled = [
            tokens[0].is_cancelled(),
            tokens[1].is_cancelled(),
            tokens[2].is_cancelled(),
        ];
        assert_eq!(cancelled.iter().filter(|&&b| b).count(), 2);

        // Client should have exactly one chunk
        let chunk = rx.recv().await.unwrap();
        assert!(!chunk.is_empty());
    }

    #[tokio::test]
    async fn closed_sink_returns_closed_error() {
        let (tx, _rx) = mpsc::channel::<bytes::Bytes>(1);
        let (sink, _tokens) = RaceSink::new(tx, 1);
        drop(_rx); // close the receiver
        let h0 = sink.handle(0);

        let result = h0.send(bytes::Bytes::from("x")).await;
        assert!(matches!(result, Err(StreamSinkError::Closed)));
    }
}
