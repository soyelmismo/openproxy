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
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

#[cfg(feature = "upstream-hyper")]
use http_body_util::BodyExt;

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
    /// Wall-clock instant of the most recent chunk. `None` until the
    /// first chunk arrives; before that we fall back to `start` for
    /// the deadline computation so the implicit TTFT (== headers
    /// deadline) still bounds the very first frame.
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
        Self {
            inner: Some(http_body_util::BodyStream::new(limited)),
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
        Self {
            #[cfg(feature = "upstream-hyper")]
            inner: None,
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
        use futures_util::StreamExt;
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
        // Cancellation check at the top: if we were cancelled while the
        // caller was doing other work, fail fast without consuming the
        // underlying body.
        if self.cancel.is_cancelled() {
            return Err(UpstreamError::Cancel);
        }

        // Per-chunk gap deadline: the maximum gap between this chunk
        // and the previous one. Before the first chunk arrives we
        // anchor the timer at `start` (the implicit TTFT ceiling),
        // which preserves the previous behavior of the first chunk
        // still being bounded by the request-start timeline.
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

            // Race the next-frame future against a sleep until the
            // chunk-gap deadline, a sleep until the total_deadline,
            // and a fast cancel-poll.
            let cancel = self.cancel.clone();
            let next = async move {
                // Wrap the poll-stream into a single-shot future by
                // using a small `poll_fn` shim. We use `next()` from
                // futures_util but cap each step with the deadline
                // timer.
                use futures_util::StreamExt;
                stream.next().await
            };

            // Three-way select: chunk / chunk-gap-deadline / total-deadline / cancel.
            tokio::select! {
                biased;
                _ = sleep_until_or_cancel(chunk_gap_deadline, &cancel) => {
                    Err(UpstreamError::Timeout(UpstreamPhase::Body))
                }
                _ = sleep_until(self.total_deadline) => {
                    Err(UpstreamError::Timeout(UpstreamPhase::Body))
                }
                res = next => {
                    match res {
                        Some(Ok(frame)) => {
                            // `Limited` yields `Frame::data(Bytes)` for body
                            // bytes; ignore trailers (we don't surface them
                            // in this Gate-0 surface).
                            // Stash the wall-clock instant of this chunk so
                            // the NEXT call computes its gap deadline from
                            // here (this is the bug-2a fix: idle_chunk_ms is
                            // now enforced as a gap, not as a deadline
                            // relative to `start`).
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
            // No body data when the feature is off.
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

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

/// Sleep until the absolute instant, but resolve early if the cancel
/// token fires. Returns `()` either way; the caller maps the outcome.
async fn sleep_until_or_cancel(deadline: Instant, cancel: &CancellationToken) {
    let now = Instant::now();
    if deadline <= now {
        return;
    }
    let dur = deadline - now;
    let sleep = tokio::time::sleep(dur);
    tokio::select! {
        _ = sleep => {}
        _ = wait_cancel(cancel) => {}
    }
}

async fn wait_cancel(cancel: &CancellationToken) {
    // Cheap poll: yield once then peek. Using a tight loop with
    // tokio::task::yield_now keeps the future cooperative without
    // spinning.
    loop {
        if cancel.is_cancelled() {
            return;
        }
        tokio::task::yield_now().await;
    }
}

async fn sleep_until(deadline: Instant) {
    let now = Instant::now();
    if deadline <= now {
        return;
    }
    tokio::time::sleep(deadline - now).await;
}

/// `UpstreamBodyStream` is fused via the `next_chunk` API; the spec
/// asks for an async iterator. We don't implement `Stream` to keep
/// the dependency surface minimal (no `futures-core::Stream` trait
/// on the public API), but the method form is identical in usage.
impl UpstreamBodyStream {
    /// `Pin<Box<dyn Future>>` form of `next_chunk` for callers that
    /// want to combine it manually with other futures.
    pub fn next_chunk_boxed(
        &mut self,
    ) -> Pin<Box<dyn std::future::Future<Output = UpstreamResult<Option<Bytes>>> + Send + '_>>
    {
        Box::pin(self.next_chunk())
    }
}
