use openproxy_adapters::upstream::CancellationToken;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StreamSinkError {
    #[error("stream sink closed")]
    Closed,
    #[error("race lost")]
    Lost,
}

/// All clones share the commit bit, including the sinks passed into race lanes.
/// Enqueuing bytes is the commit point: a receiver may consume them immediately.
#[derive(Debug, Clone)]
pub struct DirectSink {
    tx: mpsc::Sender<bytes::Bytes>,
    committed: Arc<AtomicBool>,
}

impl From<mpsc::Sender<bytes::Bytes>> for DirectSink {
    fn from(tx: mpsc::Sender<bytes::Bytes>) -> Self {
        Self {
            tx,
            committed: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl DirectSink {
    async fn send(&self, chunk: bytes::Bytes) -> Result<(), StreamSinkError> {
        let permit = self
            .tx
            .reserve()
            .await
            .map_err(|_| StreamSinkError::Closed)?;
        if !chunk.is_empty() {
            self.committed.store(true, Ordering::Release);
        }
        permit.send(chunk);
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum StreamSink {
    Direct(DirectSink),
    Race(RaceSinkHandle),
    Discard,
}

impl StreamSink {
    pub fn direct(tx: mpsc::Sender<bytes::Bytes>) -> Self {
        Self::Direct(tx.into())
    }

    /// Claim the final result through the same arbiter as the first chunk.
    /// A unary/empty success must not replace another lane's committed stream.
    pub(crate) fn claim_result(&self) -> bool {
        match self {
            Self::Race(handle) => handle.sink.claim_winner(handle.worker_id).is_ok(),
            Self::Direct(_) | Self::Discard => true,
        }
    }

    /// Whether this lane has committed bytes to the client. On the original
    /// Direct sink this covers all lanes; a Race handle covers only its owner.
    /// Discard sinks do not commit, so synchronous partial attempts can retry.
    pub fn has_output(&self) -> bool {
        match self {
            Self::Direct(sink) => sink.committed.load(Ordering::Acquire),
            Self::Race(handle) => {
                handle.sink.winner.load(Ordering::Acquire) == handle.worker_id + 1
                    && handle.sink.inner.committed.load(Ordering::Acquire)
            }
            Self::Discard => false,
        }
    }

    pub async fn send(&self, chunk: bytes::Bytes) -> Result<(), StreamSinkError> {
        match self {
            StreamSink::Direct(sink) => sink.send(chunk).await,
            StreamSink::Race(handle) => handle.send(chunk).await,
            StreamSink::Discard => Ok(()),
        }
    }
}

pub struct RaceSink {
    inner: DirectSink,
    winner: AtomicUsize,
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
    pub fn new(
        inner: impl Into<DirectSink>,
        num_workers: usize,
    ) -> (Arc<Self>, Vec<CancellationToken>) {
        let worker_tokens: Vec<CancellationToken> =
            (0..num_workers).map(|_| CancellationToken::new()).collect();
        let sink = Arc::new(Self {
            inner: inner.into(),
            winner: AtomicUsize::new(0),
            worker_tokens: worker_tokens.clone(),
        });
        (sink, worker_tokens)
    }

    pub fn handle(self: &Arc<Self>, worker_id: usize) -> RaceSinkHandle {
        RaceSinkHandle {
            sink: Arc::clone(self),
            worker_id,
        }
    }

    fn cancel_other_workers(&self, winner_id: usize) {
        for (idx, token) in self.worker_tokens.iter().enumerate() {
            if idx != winner_id {
                token.cancel();
            }
        }
    }

    fn try_register_winner(&self, target: usize, worker_id: usize) -> Result<(), StreamSinkError> {
        match self
            .winner
            .compare_exchange(0, target, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                self.cancel_other_workers(worker_id);
                Ok(())
            }
            Err(existing) if existing == target => Ok(()),
            Err(_) => Err(StreamSinkError::Lost),
        }
    }

    fn claim_winner(&self, worker_id: usize) -> Result<(), StreamSinkError> {
        let target = worker_id + 1;
        match self.winner.load(Ordering::Acquire) {
            0 => self.try_register_winner(target, worker_id),
            curr if curr == target => Ok(()),
            _ => Err(StreamSinkError::Lost),
        }
    }

    async fn send(&self, worker_id: usize, chunk: bytes::Bytes) -> Result<(), StreamSinkError> {
        self.claim_winner(worker_id)?;
        self.inner.send(chunk).await
    }
}

#[derive(Debug, Clone)]
pub struct RaceSinkHandle {
    sink: Arc<RaceSink>,
    worker_id: usize,
}

impl RaceSinkHandle {
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

        h0.send(bytes::Bytes::from("chunk0")).await.unwrap();

        assert!(h1.send(bytes::Bytes::from("chunk1")).await.is_err());

        assert!(!tokens[0].is_cancelled());
        assert!(tokens[1].is_cancelled());

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

        let (r0, r1, r2) = tokio::join!(
            h0.send(bytes::Bytes::from("0")),
            h1.send(bytes::Bytes::from("1")),
            h2.send(bytes::Bytes::from("2")),
        );

        let wins = [r0.is_ok(), r1.is_ok(), r2.is_ok()];
        assert_eq!(wins.iter().filter(|&&b| b).count(), 1, "exactly one winner");

        let cancelled = [
            tokens[0].is_cancelled(),
            tokens[1].is_cancelled(),
            tokens[2].is_cancelled(),
        ];
        assert_eq!(cancelled.iter().filter(|&&b| b).count(), 2);

        let chunk = rx.recv().await.unwrap();
        assert!(!chunk.is_empty());
    }

    #[tokio::test]
    async fn result_winner_cannot_replace_the_lane_that_sent_bytes() {
        let (tx, mut rx) = mpsc::channel(1);
        let direct = StreamSink::direct(tx);
        let StreamSink::Direct(inner) = &direct else {
            unreachable!()
        };
        let (sink, _) = RaceSink::new(inner.clone(), 2);
        let output_lane = StreamSink::Race(sink.handle(0));
        let result_lane = StreamSink::Race(sink.handle(1));
        output_lane
            .send(bytes::Bytes::from_static(b"partial"))
            .await
            .unwrap();
        assert!(direct.has_output());
        assert!(output_lane.has_output());
        assert!(!result_lane.has_output());
        assert!(
            !result_lane.claim_result(),
            "a success from another lane cannot mask the partial error"
        );
        assert!(output_lane.claim_result());
        assert_eq!(rx.recv().await.unwrap(), "partial");
    }

    #[tokio::test]
    async fn result_before_first_chunk_owns_the_same_arbiter() {
        let (tx, _rx) = mpsc::channel(1);
        let (sink, tokens) = RaceSink::new(tx, 2);
        let result_lane = StreamSink::Race(sink.handle(0));
        let output_lane = StreamSink::Race(sink.handle(1));
        assert!(result_lane.claim_result());
        assert!(tokens[1].is_cancelled());
        assert_eq!(
            output_lane.send(bytes::Bytes::from_static(b"late")).await,
            Err(StreamSinkError::Lost)
        );
    }

    #[tokio::test]
    async fn closed_sink_returns_closed_error() {
        let (tx, _rx) = mpsc::channel::<bytes::Bytes>(1);
        let (sink, _tokens) = RaceSink::new(tx, 1);
        drop(_rx);
        let h0 = sink.handle(0);

        let result = h0.send(bytes::Bytes::from("x")).await;
        assert!(matches!(result, Err(StreamSinkError::Closed)));
    }
}
