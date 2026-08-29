//! The response surface returned by `UpstreamClient::call`.
//!
//! `UpstreamResponse` carries the status, headers, and a streaming body.
//! The body is exposed as an `UpstreamBodyStream` — an async iterator
//! over `Bytes` that polls the underlying `hyper::body::Incoming` while
//! also polling the `CancellationToken` and the per-chunk deadline.

use super::cancel::CancellationToken;
use super::error::{UpstreamError, UpstreamResult};
use super::phases::UpstreamPhase;
use bytes::{Bytes, BytesMut};
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
/// maximum gap between two consecutive **content-bearing** chunks
/// (the "idle chunk" timeout), NOT as a deadline relative to the
/// request start. `last_chunk_at` tracks the instant of the most
/// recent chunk **that the caller marked as real content** via
/// [`UpstreamBodyStream::note_content_chunk`]; the next per-chunk
/// deadline is computed as `last_chunk_at + body_chunk_ms`. Until
/// the first content chunk is noted, every `next_chunk` wait is
/// bounded by `total_deadline` — so a server that opens the stream
/// with a stub event (`event: message_start`, an empty `data:` line,
/// a `: keep-alive` comment) and then goes silent is killed by
/// `total_deadline`, not by `body_chunk_ms`.
///
/// This contract exists because SSE stub events arrive as bytes on
/// the wire and would otherwise update `last_chunk_at` automatically,
/// starting the chunk-gap timer even though no real token has been
/// produced. The pipeline (which understands SSE semantics) is the
/// only entity that can decide "this chunk had real content".
pub struct UpstreamBodyStream {
    #[cfg(feature = "upstream-hyper")]
    inner: Option<http_body_util::BodyStream<http_body_util::Limited<hyper::body::Incoming>>>,
    cancel: CancellationToken,
    /// Cached watch receiver for async cancel notification.
    /// Polled via `changed()` in the hot loop — no per-chunk allocation.
    cancel_rx: watch::Receiver<bool>,
    last_chunk_at: Option<Instant>,
    body_chunk_ms: u64,
    total_deadline: Instant,
    /// When `false` (non-streaming), the body-chunk gap timeout is
    /// NOT applied. Only `total_deadline` bounds the body read.
    /// This is because non-streaming responses arrive as a single
    /// chunk — the LLM generates the full response server-side before
    /// sending anything. The `body_chunk_ms` (idle_chunk_ms) timeout
    /// is a streaming concept (max gap between SSE chunks) and doesn't
    /// apply to non-streaming.
    is_streaming: bool,
    /// PERF: reusable Sleep stored in a Box<Pin<Sleep>> to allow
    /// in-place reset without timer-wheel register/deregister per chunk.
    sleep: std::pin::Pin<Box<tokio::time::Sleep>>,
}

impl UpstreamBodyStream {
    /// Wrap a `hyper::body::Incoming` as a streaming body. The
    /// `limited` argument caps the total bytes read (use a large value
    /// for unlimited).
    ///
    /// `body_chunk_ms` is the max gap between consecutive chunks (not
    /// a deadline relative to the request start). The first chunk is
    /// bounded by `total_deadline`; subsequent chunks use the gap.
    #[cfg(feature = "upstream-hyper")]
    pub fn from_hyper(
        body: hyper::body::Incoming,
        cancel: CancellationToken,
        body_chunk_ms: u64,
        total_deadline: Instant,
        limit: u64,
        is_streaming: bool,
    ) -> Self {
        let limit_usize = usize::try_from(limit).unwrap_or(usize::MAX);
        let limited = http_body_util::Limited::new(body, limit_usize);
        let cancel_rx = cancel.subscribe();
        // For non-streaming, the initial deadline is total_deadline.
        // The LLM needs time to generate the full response before
        // sending the first (and only) chunk. For streaming, the
        // initial deadline is also total_deadline — the first chunk
        // is bounded by the headers_deadline (ttft_ms) which is
        // enforced by the upstream client's select! in call_inner.
        // The body_chunk_ms gap only applies AFTER the first chunk
        // arrives (in next_chunk's gap calc). Previously, the
        // initial deadline was start + body_chunk_ms which killed
        // streaming requests whose first token took longer than
        // body_chunk_ms (e.g. 10s) even though ttft_ms (30s) hadn't
        // expired yet.
        let initial_deadline = total_deadline;
        Self {
            inner: Some(http_body_util::BodyStream::new(limited)),
            cancel_rx,
            cancel,
            last_chunk_at: None,
            body_chunk_ms,
            total_deadline,
            is_streaming,
            sleep: Box::pin(tokio::time::sleep_until(initial_deadline.into())),
        }
    }

    /// Build a body stream that yields no data and reports a single
    /// error on first poll. Used when the upstream call fails before
    /// we have a body in hand.
    pub fn empty(
        cancel: CancellationToken,
        body_chunk_ms: u64,
        total_deadline: Instant,
        is_streaming: bool,
    ) -> Self {
        let cancel_rx = cancel.subscribe();
        let initial_deadline = total_deadline;
        Self {
            #[cfg(feature = "upstream-hyper")]
            inner: None,
            cancel_rx,
            cancel,
            last_chunk_at: None,
            body_chunk_ms,
            total_deadline,
            is_streaming,
            sleep: Box::pin(tokio::time::sleep_until(initial_deadline.into())),
        }
    }

    /// Consume the stream and collect every chunk into one `Bytes`.
    /// On cancel, returns `UpstreamError::Cancel`. On chunk-gap
    /// timeout, returns `UpstreamError::Timeout(Body)`. On total
    /// timeout, returns `UpstreamError::Timeout(Body)` (the caller can
    /// see the start instant + `now` vs. `total_deadline` to
    /// disambiguate; we only carry one phase here for simplicity).
    pub async fn collect_all(mut self) -> UpstreamResult<Bytes> {
        let Some(first_chunk) = self.next_chunk().await? else {
            return Ok(Bytes::new());
        };
        let Some(second_chunk) = self.next_chunk().await? else {
            return Ok(first_chunk);
        };
        let mut chunks = vec![first_chunk, second_chunk];
        let mut total_len = chunks[0].len() + chunks[1].len();
        while let Some(chunk) = self.next_chunk().await? {
            total_len += chunk.len();
            chunks.push(chunk);
        }
        let mut buf = BytesMut::with_capacity(total_len);
        for chunk in chunks {
            buf.extend_from_slice(&chunk);
        }
        Ok(buf.freeze())
    }

    /// Yield the next chunk. Returns `Ok(None)` at end of stream.
    ///
    /// Honors `cancel`, the body-chunk **gap** deadline (recomputed
    /// after every **content-bearing** chunk as
    /// `last_chunk_at + body_chunk_ms`, with `total_deadline` as the
    /// ceiling while no content chunk has been noted), and the
    /// `total_deadline`.
    ///
    /// **Contract**: the caller MUST call [`note_content_chunk`]
    /// after parsing a chunk that carries real content (token delta,
    /// tool-call fragment, etc.). Until that first call, every
    /// `next_chunk` wait is bounded by `total_deadline` only —
    /// `body_chunk_ms` does NOT apply. This is intentional: a stub
    /// SSE event (`event: message_start`, empty `data:`) that
    /// arrives before the first real token should NOT start the
    fn compute_min_deadline(
        is_streaming: bool,
        last_chunk_at: Option<Instant>,
        body_chunk_ms: u64,
        total_deadline: Instant,
    ) -> Instant {
        if !is_streaming {
            return total_deadline;
        }
        match last_chunk_at {
            Some(last) => {
                let chunk_gap_deadline = last + Duration::from_millis(body_chunk_ms);
                std::cmp::min(chunk_gap_deadline, total_deadline)
            }
            None => total_deadline,
        }
    }

    fn timeout_error_for_gap(last_chunk_at: Option<Instant>) -> UpstreamError {
        if last_chunk_at.is_some() {
            UpstreamError::Timeout(UpstreamPhase::Body)
        } else {
            UpstreamError::Timeout(UpstreamPhase::Total)
        }
    }

    #[cfg(feature = "upstream-hyper")]
    fn map_stream_frame<E: std::fmt::Display>(
        res: Option<Result<hyper::body::Frame<Bytes>, E>>,
    ) -> UpstreamResult<Option<Bytes>> {
        match res {
            Some(Ok(frame)) => Ok(Some(frame.into_data().unwrap_or_default())),
            Some(Err(e)) => Err(UpstreamError::Http(e.to_string())),
            None => Ok(None),
        }
    }

    /// Pull the next chunk from the upstream body stream.
    ///
    /// Returns `Ok(Some(chunk))` on data, `Ok(None)` on EOF,
    /// `Err(Cancel)` on cancellation, `Err(Timeout(Body))` on
    /// chunk-gap expiry, `Err(Timeout(Total))` on total-deadline
    /// expiry, or `Err(Http)` on stream error.
    ///
    /// Note: the caller must call [`note_content_chunk`] after
    /// receiving a chunk that contains real model output (e.g. a
    /// non-empty SSE data line). Only real content resets the
    /// chunk-gap timer; the request should be bounded by
    /// `total_deadline` until real content flows.
    ///
    /// [`note_content_chunk`]: Self::note_content_chunk
    pub async fn next_chunk(&mut self) -> UpstreamResult<Option<Bytes>> {
        if self.cancel.is_cancelled() {
            return Err(UpstreamError::Cancel);
        }

        let min_deadline = Self::compute_min_deadline(
            self.is_streaming,
            self.last_chunk_at,
            self.body_chunk_ms,
            self.total_deadline,
        );

        #[cfg(feature = "upstream-hyper")]
        {
            let Some(stream) = self.inner.as_mut() else {
                return Ok(None);
            };

            // PERF: reset the reusable Sleep instead of creating a fresh
            // sleep_until future. `reset()` updates the timer-wheel entry
            // in place — no heap allocation, no register/deregister cycle.
            self.sleep.as_mut().reset(min_deadline.into());

            tokio::select! {
                biased;
                _ = self.cancel_rx.changed() => {
                    Err(UpstreamError::Cancel)
                }
                () = &mut self.sleep => {
                    Err(Self::timeout_error_for_gap(self.last_chunk_at))
                }
                res = futures_util::StreamExt::next(stream) => {
                    Self::map_stream_frame(res)
                }
            }
        }

        #[cfg(not(feature = "upstream-hyper"))]
        {
            let _ = (min_deadline, self.body_chunk_ms, self.is_streaming);
            Ok(None)
        }
    }

    /// Mark the most recent chunk as "real content" — i.e. the chunk
    /// carried a token delta, tool-call fragment, or other meaningful
    /// payload that the pipeline actually forwarded to the client.
    ///
    /// This resets the chunk-gap (`body_chunk_ms`) timer: subsequent
    /// `next_chunk()` calls will use `last_chunk_at + body_chunk_ms`
    /// as the per-chunk deadline (clamped by `total_deadline`).
    ///
    /// Until this method is called at least once, `next_chunk()`
    /// uses `total_deadline` for every wait — so stub events
    /// (`event: message_start`, empty `data:` lines, `:` comments)
    /// that arrive before the first real content chunk do NOT start
    /// the idle-chunk timer. A server that opens the stream with a
    /// stub event and then goes silent is killed by `total_deadline`,
    /// not by `body_chunk_ms`.
    ///
    /// Call this from the streaming loop AFTER successfully parsing
    /// and emitting a content-bearing SSE event. For Anthropic-shaped
    /// upstreams that means `content_block_delta` (text_delta,
    /// input_json_delta, thinking_delta). For OpenAI-shaped upstreams
    /// that means `choices[0].delta` with non-empty `content`,
    /// `tool_calls`, or `reasoning_content`.
    pub fn note_content_chunk(&mut self) {
        self.last_chunk_at = Some(Instant::now());
    }
}

impl std::fmt::Debug for UpstreamBodyStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamBodyStream")
            .field("cancelled", &self.cancel.is_cancelled())
            .finish_non_exhaustive()
    }
}
