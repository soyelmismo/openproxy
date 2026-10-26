//! Downstream SSE write coalescing.
//!
//! Every item handed to the HTTP body becomes its own socket write. With one
//! SSE event per item a fast upstream costs one `writev` (and one pass through
//! the kernel network stack) per event. This wrapper joins the frames that
//! arrive within a short window into a single body chunk.
//!
//! Guarantees:
//! - The first frame is never delayed (TTFT unchanged).
//! - A frame waits at most `window` before it is written, whether the upstream
//!   goes idle (timer) or stays ready without pause (checked every frame).
//! - Streams never share buffers: each request owns its wrapper, so a stalled
//!   stream cannot delay or drop frames of another.
//! - Buffered bytes are bounded by [`MAX_BUFFER_BYTES`]; a full buffer flushes
//!   immediately, so the pipeline keeps its backpressure.
//! - Frames are concatenated verbatim, never split or reordered, so the byte
//!   stream seen by the client is identical. Only chunk boundaries change,
//!   which SSE clients must already tolerate (TCP gives no framing guarantee).
//! - End of stream flushes whatever is pending, including terminal frames.

use bytes::{Bytes, BytesMut};
use futures::stream::Stream;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::{Instant, Sleep};

/// Flush once this many bytes are pending, regardless of the window.
pub const MAX_BUFFER_BYTES: usize = 64 * 1024;

/// Default coalescing window applied when the environment does not override it.
/// A 0-20 ms sweep at 800 streams put the best throughput and proxy CPU at 1 ms;
/// wider windows only cut writes further while adding per-frame delay.
pub const DEFAULT_WINDOW: Duration = Duration::from_millis(1);

/// Window from `OPENPROXY_SSE_COALESCE_MS` (milliseconds, `0` disables),
/// read once per process.
pub fn configured_window() -> Duration {
    static WINDOW: OnceLock<Duration> = OnceLock::new();
    *WINDOW.get_or_init(|| {
        std::env::var("OPENPROXY_SSE_COALESCE_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(DEFAULT_WINDOW, Duration::from_millis)
    })
}

pub struct CoalescedSse<S> {
    inner: S,
    window: Duration,
    buffer: BytesMut,
    /// When the oldest buffered frame must be written. `Some` exactly while
    /// `buffer` is non-empty. This is the single source of truth for the
    /// window: it is checked on every loop turn, so a frame is bounded by the
    /// window whether the upstream goes idle or stays ready forever.
    deadline: Option<Instant>,
    /// Wakes the task when the deadline passes while the upstream is idle.
    /// Only armed right before returning `Pending`.
    timer: Pin<Box<Sleep>>,
    first_sent: bool,
    finished: bool,
}

impl<S> CoalescedSse<S> {
    pub fn new(inner: S, window: Duration) -> Self {
        Self {
            inner,
            window,
            buffer: BytesMut::new(),
            deadline: None,
            timer: Box::pin(tokio::time::sleep_until(Instant::now())),
            first_sent: false,
            finished: false,
        }
    }

    fn take(&mut self) -> Bytes {
        self.deadline = None;
        self.buffer.split().freeze()
    }

    fn take_pending(&mut self) -> Option<Bytes> {
        (!self.buffer.is_empty()).then(|| self.take())
    }

    fn deadline_passed(&self) -> bool {
        self.deadline.is_some_and(|d| Instant::now() >= d)
    }
}

impl<S> Stream for CoalescedSse<S>
where
    S: Stream<Item = Result<Bytes, Infallible>> + Unpin,
{
    type Item = Result<Bytes, Infallible>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.window.is_zero() {
            return Pin::new(&mut this.inner).poll_next(cx);
        }
        if this.finished {
            return Poll::Ready(this.take_pending().map(Ok));
        }
        loop {
            // Enforced before every upstream poll: a ready-forever upstream
            // can never stretch the window.
            if this.deadline_passed() {
                return Poll::Ready(Some(Ok(this.take())));
            }
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    if !this.first_sent {
                        // TTFT: the first frame always leaves at once.
                        this.first_sent = true;
                        return Poll::Ready(Some(Ok(frame)));
                    }
                    if frame.is_empty() {
                        continue;
                    }
                    if this.buffer.is_empty() {
                        if frame.len() >= MAX_BUFFER_BYTES {
                            return Poll::Ready(Some(Ok(frame)));
                        }
                        this.deadline = Some(Instant::now() + this.window);
                    }
                    this.buffer.extend_from_slice(&frame);
                    if this.buffer.len() >= MAX_BUFFER_BYTES {
                        return Poll::Ready(Some(Ok(this.take())));
                    }
                }
                Poll::Ready(Some(Err(never))) => match never {},
                Poll::Ready(None) => {
                    this.finished = true;
                    return Poll::Ready(this.take_pending().map(Ok));
                }
                Poll::Pending => {
                    let Some(deadline) = this.deadline else {
                        return Poll::Pending;
                    };
                    // Upstream idle with frames pending: arm the timer so the
                    // task is woken at the deadline even if nothing else arrives.
                    if this.timer.deadline() != deadline {
                        this.timer.as_mut().reset(deadline);
                    }
                    return match this.timer.as_mut().poll(cx) {
                        Poll::Ready(()) => Poll::Ready(Some(Ok(this.take()))),
                        Poll::Pending => Poll::Pending,
                    };
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "sse_coalesce_tests.rs"]
mod tests;
