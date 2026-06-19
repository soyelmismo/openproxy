//! The response surface returned by `UpstreamClient::call`.
//!
//! `UpstreamResponse` carries the status, headers, and a streaming body.
//! The body is exposed as an `UpstreamBodyStream` — an async iterator
//! over `Bytes` that polls the underlying `hyper::body::Incoming` while
//! also polling the `CancellationToken` and the per-chunk deadline.

use super::cancel::CancellationToken;
use super::error::{UpstreamError, UpstreamResult};
use super::phases::UpstreamPhase;
use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// The response returned by `UpstreamClient::call`.
#[derive(Debug)]
pub struct UpstreamResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    /// Streaming body. May be empty (zero frames) for 204/304.
    pub body: UpstreamBodyStream,
}

impl UpstreamResponse {
    /// Collect the entire body into a single `Bytes`. Honors the
    /// `cancel` token at every chunk. For very large bodies prefer
    /// `body` directly.
    pub async fn collect(self) -> UpstreamResult<Bytes> {
        self.body.collect_all().await
    }
}

// -----------------------------------------------------------------------
// Body stream
// -----------------------------------------------------------------------

/// An async stream of `Bytes` chunks coming back from the upstream.
///
/// Internally wraps a `hyper::body::Incoming` (when the `upstream-hyper`
/// feature is on) and yields `Result<Bytes, UpstreamError>` so the
/// caller can `?`-propagate errors without juggling two error types.
///
/// Cancellation: `UpstreamBodyStream` polls the `CancellationToken`
/// between frames. The token does not need to live longer than the
/// stream; the stream holds a clone.
///
/// Body-chunk timeout semantics: `body_chunk_ms` is enforced as the
/// maximum gap between two consecutive chunks (the "idle chunk"
/// timeout), NOT as a deadline relative to the request start.
/// `last_chunk_at` tracks the instant of the most recent chunk; the
/// next per-chunk deadline is computed as
/// `last_chunk_at + body_chunk_ms`. For the first chunk we fall back
/// to `start`, which preserves the implicit TTFT ceiling: a server
/// that never produces the first chunk will be killed by the
/// `headers_deadline` (applied in `client::call_inner`) before the
/// per-chunk gap ever starts being measured.
pub struct UpstreamBodyStream {
    #[cfg(feature = "upstream-hyper")]
    inner: Option<http_body_util::BodyStream<http_body_util::Limited<hyper::body::Incoming>>>,
    cancel: CancellationToken,
    /// Cached watch receiver for async cancel notification.
    /// Polled via `changed()` in the hot loop — no per-chunk allocation.
    cancel_rx: watch::Receiver<bool>,
    last_chunk_at: Option<Instant>,
    start: Instant,
    body_chunk_ms: u64,
    total_deadline: Instant,
}

impl UpstreamBodyStream {
    /// Wrap a `hyper::body::Incoming` as a streaming body. The
    /// `limited` argument caps the total bytes read (use a large value
    /// for unlimited).
    ///
    /// `start` is the wall-clock instant the upstream call began; the
    /// body-chunk gap timer falls back to it before the first chunk
    /// arrives (preserves the implicit TTFT ceiling). `body_chunk_ms`
    /// is the max gap between consecutive chunks (not a deadline
    /// relative to `start`).
    #[cfg(feature = "upstream-hyper")]
    pub fn from_hyper(
        body: hyper::body::Incoming,
        cancel: CancellationToken,
        start: Instant,
        body_chunk_ms: u64,
        total_deadline: Instant,
        limit: u64,
    ) -> Self {
        let limit_usize = usize::try_from(limit).unwrap_or(usize::MAX);
        let limited = http_body_util::Limited::new(body, limit_usize);
        let cancel_rx = cancel.subscribe();
        Self {
            inner: Some(http_body_util::BodyStream::new(limited)),
            cancel_rx,
            cancel,
            last_chunk_at: None,
            start,
            body_chunk_ms,
            total_deadline,
        }
    }

    /// Build a body stream that yields no data and reports a single
    /// error on first poll. Used when the upstream call fails before
    /// we have a body in hand.
    pub fn empty(cancel: CancellationToken, start: Instant, body_chunk_ms: u64, total_deadline: Instant) -> Self {
        let cancel_rx = cancel.subscribe();
        Self {
            #[cfg(feature = "upstream-hyper")]
            inner: None,
            cancel_rx,
            cancel,
            last_chunk_at: None,
            start,
            body_chunk_ms,
            total_deadline,
        }
    }

    /// Consume the stream and collect every chunk into one `Bytes`.
    /// On cancel, returns `UpstreamError::Cancel`. On chunk-gap
    /// timeout, returns `UpstreamError::Timeout(Body)`. On total
    /// timeout, returns `UpstreamError::Timeout(Body)` (the caller can
    /// see the start instant + `now` vs. `total_deadline` to
    /// disambiguate; we only carry one phase here for simplicity).
    pub async fn collect_all(mut self) -> UpstreamResult<Bytes> {
        let mut buf = Vec::new();
        while let Some(chunk) = self.next_chunk().await? {
            buf.extend_from_slice(&chunk);
        }
        Ok(Bytes::from(buf))
    }

    /// Yield the next chunk. Returns `Ok(None)` at end of stream.
    ///
    /// Honors `cancel`, the body-chunk **gap** deadline (recomputed
    /// after every chunk as `last_chunk_at + body_chunk_ms`, with
    /// `start` as the fallback for the very first chunk), and the
    /// `total_deadline`.
    pub async fn next_chunk(&mut self) -> UpstreamResult<Option<Bytes>> {
        if self.cancel.is_cancelled() {
            return Err(UpstreamError::Cancel);
        }

        let chunk_gap_deadline = self
            .last_chunk_at
            .unwrap_or(self.start)
            + Duration::from_millis(self.body_chunk_ms);

        #[cfg(feature = "upstream-hyper")]
        {
            let stream = match self.inner.as_mut() {
                Some(s) => s,
                None => return Ok(None),
            };

            let min_deadline = std::cmp::min(chunk_gap_deadline, self.total_deadline);

            tokio::select! {
                biased;
                // Poll the cached watch receiver — no allocation,
                // just a version-counter check.
                _ = self.cancel_rx.changed() => {
                    Err(UpstreamError::Cancel)
                }
                _ = tokio::time::sleep_until(min_deadline.into()) => {
                    Err(UpstreamError::Timeout(UpstreamPhase::Body))
                }
                res = futures_util::StreamExt::next(stream) => {
                    match res {
                        Some(Ok(frame)) => {
                            self.last_chunk_at = Some(Instant::now());
                            Ok(Some(frame.into_data().unwrap_or_default()))
                        }
                        Some(Err(e)) => Err(UpstreamError::Http(e.to_string())),
                        None => Ok(None),
                    }
                }
            }
        }

        #[cfg(not(feature = "upstream-hyper"))]
        {
            let _ = (chunk_gap_deadline, self.body_chunk_ms);
            Ok(None)
        }
    }
}

impl std::fmt::Debug for UpstreamBodyStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamBodyStream")
            .field("cancelled", &self.cancel.is_cancelled())
            .finish()
    }
}
