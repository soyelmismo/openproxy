use openproxy_adapters::upstream::CancellationToken;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::mpsc;

#[derive(Debug, thiserror::Error)]
pub enum StreamSinkError {
    #[error("stream sink closed")]
    Closed,
    #[error("race lost")]
    Lost,
}

#[derive(Debug, Clone)]
pub enum StreamSink {
    Direct(mpsc::Sender<bytes::Bytes>),
    Race(RaceSinkHandle),
    Discard,
}

impl StreamSink {
    pub async fn send(&self, chunk: bytes::Bytes) -> Result<(), StreamSinkError> {
        match self {
            StreamSink::Direct(tx) => tx.send(chunk).await.map_err(|_| StreamSinkError::Closed),
            StreamSink::Race(handle) => handle.send(chunk).await,
            StreamSink::Discard => Ok(()),
        }
    }
}

pub struct RaceSink {
    inner: mpsc::Sender<bytes::Bytes>,
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

    pub fn handle(self: &Arc<Self>, worker_id: usize) -> RaceSinkHandle {
        RaceSinkHandle {
            sink: self.clone(),
            worker_id,
        }
    }

    async fn send(&self, worker_id: usize, chunk: bytes::Bytes) -> Result<(), StreamSinkError> {
        let current = self.winner.load(Ordering::Acquire);
        if current != 0 && current != worker_id + 1 {
            return Err(StreamSinkError::Lost);
        }

        if current == 0 {
            match self.winner.compare_exchange(
                0,
                worker_id + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
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
                }
            }
        }

        self.inner
            .send(chunk)
            .await
            .map_err(|_| StreamSinkError::Closed)
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
    async fn closed_sink_returns_closed_error() {
        let (tx, _rx) = mpsc::channel::<bytes::Bytes>(1);
        let (sink, _tokens) = RaceSink::new(tx, 1);
        drop(_rx);
        let h0 = sink.handle(0);

        let result = h0.send(bytes::Bytes::from("x")).await;
        assert!(matches!(result, Err(StreamSinkError::Closed)));
    }
}
