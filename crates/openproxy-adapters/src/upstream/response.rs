//! The response surface returned by `UpstreamClient::call`.
//!
//! `UpstreamResponse` carries the status, headers, and a streaming body.
//! The body is exposed as an `UpstreamBodyStream` — an async iterator
//! over `Bytes` that polls the underlying `hyper::body::Incoming` while
//! also polling the `CancellationToken` and the per-chunk deadline.

use super::cancel::CancellationToken;
use super::compression::BodyDecoding;
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
    /// Collect the entire body into a single `Bytes`, honoring the `cancel`
    /// token at every chunk. Prefer `body` directly for large payloads.
    pub async fn collect(self) -> UpstreamResult<Bytes> {
        self.body.collect_all().await
    }
}

/// Default body limit for non-streaming upstream responses (32 MiB).
/// Buffers the complete payload in memory for downstream deserialization.
pub const NON_STREAMING_BODY_LIMIT_BYTES: u64 = 32 * 1024 * 1024;

/// Body limit for streaming upstream responses (`u64::MAX` / unlimited).
/// Piped chunk-by-chunk to the client with no unbounded heap growth, and
/// governed by phase deadlines and idle-chunk timeouts.
pub const STREAMING_BODY_LIMIT_BYTES: u64 = u64::MAX;

// Body stream

/// An async stream of `Bytes` chunks coming back from the upstream.
///
/// Wraps a `hyper::body::Incoming` (when the `upstream-hyper` feature is on)
/// and yields `Result<Bytes, UpstreamError>` so the caller can `?`-propagate
/// errors without juggling two error types. Polls the `CancellationToken`
/// between frames; the token need not outlive the stream, which holds a clone.
///
/// `body_chunk_ms` bounds the gap between two consecutive **content-bearing**
/// chunks, not the time since the request started. `last_chunk_at` tracks the
/// latest chunk the caller marked as real content via
/// [`UpstreamBodyStream::note_content_chunk`], and the next per-chunk deadline
/// is `last_chunk_at + body_chunk_ms`. Until the first content chunk is noted
/// every `next_chunk` wait is bounded by `total_deadline`.
///
/// Stub SSE events (`event: message_start`, an empty `data:` line, a
/// `:` keep-alive comment) arrive as bytes on the wire and would reset
/// `last_chunk_at` on their own, starting the chunk-gap timer before any real
/// token. Only the pipeline, which understands SSE semantics, can tell whether
/// a chunk carried content.
pub struct UpstreamBodyStream {
    #[cfg(feature = "upstream-hyper")]
    inner: Option<http_body_util::BodyStream<http_body_util::Limited<hyper::body::Incoming>>>,
    cancel: CancellationToken,
    /// Cached watch receiver for async cancel notification.
    /// Polled via `changed()` in the hot loop, no per-chunk allocation.
    cancel_rx: watch::Receiver<bool>,
    /// Inverts `content-encoding` before the caller sees any bytes. The
    /// default is a passthrough, so an uncompressed response behaves exactly
    /// as it did before this field existed.
    decoding: BodyDecoding,
    last_chunk_at: Option<Instant>,
    body_chunk_ms: u64,
    ttft_deadline: Instant,
    total_deadline: Instant,
    /// When `false` (non-streaming), the body-chunk gap timeout is NOT
    /// applied and only `total_deadline` bounds the read: such a response
    /// arrives as one chunk, because the LLM generates the full reply
    /// server-side before sending anything.
    is_streaming: bool,
    /// Reusable `Sleep` behind `Pin<Box<_>>` for in-place reset without a
    /// timer-wheel register/deregister cycle per chunk.
    sleep: std::pin::Pin<Box<tokio::time::Sleep>>,
}

impl UpstreamBodyStream {
    /// Wrap a `hyper::body::Incoming`, capping the total bytes read, and
    /// install the content-decoding implied by the response headers.
    ///
    /// `body_chunk_ms` is the max gap between consecutive chunks, not a
    /// deadline relative to the request start: the first chunk is bounded by
    /// `ttft_deadline`, later ones by the gap.
    #[cfg(feature = "upstream-hyper")]
    pub(crate) fn from_hyper(
        body: hyper::body::Incoming,
        decoding: BodyDecoding,
        cancel: CancellationToken,
        body_chunk_ms: u64,
        deadlines: (Instant, Instant),
        limit: u64,
        is_streaming: bool,
    ) -> Self {
        let (ttft_deadline, total_deadline) = deadlines;
        let limit_usize = usize::try_from(limit).unwrap_or(usize::MAX);
        let limited = http_body_util::Limited::new(body, limit_usize);
        let cancel_rx = cancel.subscribe();
        let initial_deadline = if is_streaming {
            std::cmp::min(ttft_deadline, total_deadline)
        } else {
            total_deadline
        };
        Self {
            inner: Some(http_body_util::BodyStream::new(limited)),
            cancel_rx,
            cancel,
            decoding,
            last_chunk_at: None,
            body_chunk_ms,
            ttft_deadline,
            total_deadline,
            is_streaming,
            sleep: Box::pin(tokio::time::sleep_until(initial_deadline.into())),
        }
    }

    /// A body stream that yields no data, for when the upstream call fails
    /// before a body is in hand.
    ///
    /// `pub(crate)` because [`BodyDecoding`] is crate-private decode state:
    /// exposing a constructor that takes it would widen the public API past
    /// what callers can construct. No caller uses it today; kept for the
    /// failed-call path.
    #[allow(dead_code)]
    pub(crate) fn empty(
        decoding: BodyDecoding,
        cancel: CancellationToken,
        body_chunk_ms: u64,
        ttft_deadline: Instant,
        total_deadline: Instant,
        is_streaming: bool,
    ) -> Self {
        let cancel_rx = cancel.subscribe();
        let initial_deadline = if is_streaming {
            std::cmp::min(ttft_deadline, total_deadline)
        } else {
            total_deadline
        };
        Self {
            #[cfg(feature = "upstream-hyper")]
            inner: None,
            cancel_rx,
            cancel,
            decoding,
            last_chunk_at: None,
            body_chunk_ms,
            ttft_deadline,
            total_deadline,
            is_streaming,
            sleep: Box::pin(tokio::time::sleep_until(initial_deadline.into())),
        }
    }

    /// Collect every chunk into one `Bytes`.
    ///
    /// `Err(Cancel)` on cancel; on chunk-gap or total timeout both report
    /// `Timeout(Body)`, since the caller can disambiguate from the request
    /// start instant against `total_deadline`.
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

    /// Deadline for the next `next_chunk` wait: `total_deadline` alone when
    /// non-streaming, else the chunk gap since the last content chunk clamped
    /// by `total_deadline`, or the TTFT deadline before any content chunk.
    fn compute_min_deadline(
        is_streaming: bool,
        last_chunk_at: Option<Instant>,
        body_chunk_ms: u64,
        ttft_deadline: Instant,
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
            None => std::cmp::min(ttft_deadline, total_deadline),
        }
    }

    fn timeout_error_for_gap(
        last_chunk_at: Option<Instant>,
        total_deadline: Instant,
    ) -> UpstreamError {
        let now = Instant::now();
        if last_chunk_at.is_some() {
            if now + Duration::from_millis(5) >= total_deadline {
                UpstreamError::Timeout(UpstreamPhase::Total)
            } else {
                UpstreamError::Timeout(UpstreamPhase::Body)
            }
        } else if now + Duration::from_millis(5) >= total_deadline {
            UpstreamError::Timeout(UpstreamPhase::Total)
        } else {
            UpstreamError::Timeout(UpstreamPhase::Headers)
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

    /// Yield the next chunk: `Ok(Some(chunk))` on data, `Ok(None)` on EOF,
    /// `Err(Cancel)`, `Err(Timeout(Body))` on chunk-gap expiry,
    /// `Err(Timeout(Total))` on total-deadline expiry, `Err(Http)` on stream
    /// error, `Err(Decode)` when the wire format could not be inverted.
    ///
    /// When the response carried a `content-encoding`, every frame is inflated
    /// here — before the caller's parser and before error classification — so
    /// downstream code only ever sees plaintext. Decoding happens between
    /// polls, not instead of them, so the cancel token and the chunk/total
    /// deadlines keep bounding the read exactly as before.
    ///
    /// The caller calls [`note_content_chunk`] after parsing a chunk that
    /// carries real model output. Only real content resets the chunk-gap
    /// timer, so the request stays bounded by `total_deadline` until then.
    ///
    /// [`note_content_chunk`]: Self::note_content_chunk
    pub async fn next_chunk(&mut self) -> UpstreamResult<Option<Bytes>> {
        loop {
            if self.cancel.is_cancelled() {
                return Err(UpstreamError::Cancel);
            }

            let min_deadline = Self::compute_min_deadline(
                self.is_streaming,
                self.last_chunk_at,
                self.body_chunk_ms,
                self.ttft_deadline,
                self.total_deadline,
            );

            #[cfg(feature = "upstream-hyper")]
            {
                if Instant::now() >= min_deadline {
                    return Err(Self::timeout_error_for_gap(
                        self.last_chunk_at,
                        self.total_deadline,
                    ));
                }
                // Drain bounded decoder output before waiting for another wire
                // frame. A live SSE connection may not send more input yet.
                if let Some(decoded) = self.decoding.take_frame() {
                    return Ok(Some(decoded));
                }
                if self.decoding.can_drain_without_input() {
                    self.decoding.push(&[])?;
                    continue;
                }
                let Some(stream) = self.inner.as_mut() else {
                    return self.decoding.finish();
                };

                // Reset the reusable Sleep in place instead of building a fresh
                // `sleep_until`: no heap allocation, no register/deregister cycle.
                self.sleep.as_mut().reset(min_deadline.into());

                let frame: UpstreamResult<Option<Bytes>> = tokio::select! {
                    biased;
                    _ = self.cancel_rx.changed() => Err(UpstreamError::Cancel),
                    () = &mut self.sleep => {
                        Err(Self::timeout_error_for_gap(self.last_chunk_at, self.total_deadline))
                    }
                    res = futures_util::StreamExt::next(stream) => {
                        Self::map_stream_frame(res)
                    }
                };

                // EOF: drain whatever the inflater still holds back before
                // declaring the stream over, and surface that plaintext.
                let Some(raw) = frame? else {
                    self.inner = None;
                    return self.decoding.finish();
                };

                // Identity responses already own the transport buffer. Do not
                // copy it into the decoder only to split it straight back out.
                // Compressed bodies still go through every validation below.
                if !self.decoding.is_decoding() {
                    return Ok(Some(raw));
                }

                // A frame that carried no input cannot produce plaintext
                // either (trailers-only frames); hand it back untouched.
                let carried_input = !raw.is_empty();
                self.decoding.push(&raw)?;
                if let Some(decoded) = self.decoding.take_frame() {
                    return Ok(Some(decoded));
                }
                if !carried_input {
                    return Ok(Some(Bytes::new()));
                }
                // The next iteration drains pending decoder output or polls
                // for missing input, while checking cancellation and deadlines.
            }

            #[cfg(not(feature = "upstream-hyper"))]
            {
                let _ = (min_deadline, self.body_chunk_ms, self.is_streaming);
                return Ok(None);
            }
        }
    }

    /// Mark the most recent chunk as real content (token delta, tool-call
    /// fragment, or other payload the pipeline forwarded), resetting the
    /// chunk-gap timer to `last_chunk_at + body_chunk_ms` clamped by
    /// `total_deadline`.
    ///
    /// Until the first call, `next_chunk` bounds every wait by
    /// `total_deadline`: stub events before the first real content chunk do not
    /// start the idle-chunk timer, so a server that opens the stream and goes
    /// silent dies on `total_deadline`.
    ///
    /// Call it after parsing and emitting a content-bearing SSE event:
    /// `content_block_delta` (text_delta, input_json_delta, thinking_delta)
    /// for Anthropic-shaped upstreams, or `choices[0].delta` with non-empty
    /// `content`, `tool_calls` or `reasoning_content` for OpenAI-shaped ones.
    pub fn note_content_chunk(&mut self) {
        self.last_chunk_at = Some(Instant::now());
    }

    /// Mark verified generation progress without exposing model output.
    /// Pings, block starts, zero/invalid estimates must not call this.
    /// Switches from TTFT to the idle budget, never extending total or
    /// claiming that client-visible content has arrived.
    pub fn note_progress_chunk(&mut self) {
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
