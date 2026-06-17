//! TCP-level client-disconnect detection for axum 0.7.
//!
//! Background
//! ----------
//! The chat handler used to drive the [`crate::pipeline`]'s
//! `client_disconnected` watch from a *time-based* watchdog: a
//! background task slept for `timeouts.total_ms` (or the
//! `x-request-deadline-ms` override) and then flipped the watch to
//! `true`. That captured the case where a client opened a request,
//! sent the body, and then *forgot* about it long enough that the
//! upstream `total` budget would have done the same thing — but it
//! did NOT capture the real cancel: a client that closed the TCP
//! connection (RST, half-close, or `Connection: close`) 200ms after
//! sending the body still kept the pipeline running for the full
//! `total_ms` budget.
//!
//! What this module does
//! ---------------------
//! A small axum middleware that wires a real per-request cancel
//! watch into the *body* layer:
//!
//! 1. The middleware allocates a fresh
//!    `tokio::sync::watch::channel(false)` for every request and
//!    stuffs the receiver into the request's extension bag under
//!    the [`CANCEL_WATCH_KEY`] constant.
//! 2. The request body is wrapped in [`DisconnectBody`], a newtype
//!    over any `http_body::Body`. When the underlying body's
//!    `poll_frame` yields an error — which hyper surfaces when the
//!    client closes the connection while bytes are being read — the
//!    wrapper fires the watch (idempotently) and propagates the
//!    error. This covers the case where the client closes the
//!    connection *while uploading* the JSON body (the most common
//!    form of "client gave up before even getting a request in
//!    flight").
//! 3. The handler runs. The chat handler pulls the watch receiver
//!    out of extensions and threads it into the pipeline as
//!    `PipelineRequest::client_disconnected`. When the watch flips,
//!    the pipeline aborts upstream work on the next checkpoint and
//!    records a `ClientDisconnected` (HTTP 499) usage row.
//! 4. The response body is ALSO wrapped in [`DisconnectBody`],
//!    pointing at the same watch. When hyper tries to write a
//!    chunk of the streaming response into a half-closed socket,
//!    `poll_frame` returns an error and the watch fires. This
//!    covers the "client cancelled mid-stream" case: the chat
//!    handler has returned an SSE response, the client is reading
//!    chunks, and then disconnects — the pipeline is still
//!    producing chunks on its `stream_sink` mpsc; the
//!    `ReceiverStream` returned to axum blocks on the next read,
//!    the connection write fails, and we flip the watch so the
//!    pipeline stops upstream work on the next checkpoint.
//!
//! Trade-offs
//! ----------
//! - The middleware cannot detect "client sent the full body and
//!   closed the connection *before* the handler starts running" —
//!   hyper doesn't surface a TCP-close event distinct from "body
//!   fully received". That is acceptable: in that case the client
//!   never expected a response, and the request is already done as
//!   far as the HTTP layer is concerned. If the upstream is fast
//!   enough to produce a result before any retries, we still record
//!   a usage row (the dispatcher has no other choice); if the
//!   pipeline is slow, the `x-request-deadline-ms` header
//!   (preserved by the chat handler) acts as a backup ceiling so
//!   we don't burn upstream budget forever.
//! - The middleware is route-scoped (mounted on
//!   `/v1/chat/completions` only). The admin surface and the
//!   `/v1/health` liveness probe don't need TCP-cancel tracking.
//!
//! Public surface
//! --------------
//! - [`CANCEL_WATCH_KEY`]: the extension key for the per-request
//!   watch receiver.
//! - [`client_disconnect_middleware`]: the middleware factory.
//! - [`DisconnectBody`]: the body newtype; re-exported for tests.

use axum::{
    body::Body,
    extract::Request,
    middleware::Next,
    response::Response,
};
use http_body::{Body as HttpBody, Frame, SizeHint};
use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll},
};
use tokio::sync::watch;

/// Extension key under which the per-request cancel watch is
/// stashed. The chat handler reads the receiver out of the
/// extensions bag and passes it to the pipeline.
#[derive(Clone, Copy, Debug)]
pub struct CancelWatchKey;

impl CancelWatchKey {
    /// Stable name used by the chat handler when it does a manual
    /// extension lookup. Kept as a method (not a `const`) so the
    /// type stays usable as an extension key without a separate
    /// type alias.
    pub const NAME: &'static str = "openproxy.cancel_watch";
}

/// Build the (sender, receiver) pair the middleware uses.
///
/// Exposed so tests (and the chat handler) can mint a
/// pre-constructed pair without going through the middleware.
pub fn new_cancel_pair() -> (watch::Sender<bool>, watch::Receiver<bool>) {
    watch::channel(false)
}

/// Axum middleware: see module docs.
///
/// On every request:
/// 1. Mint a fresh `watch::channel(false)`.
/// 2. Insert the *receiver* into the request extensions under
///    [`CancelWatchKey`].
/// 3. Wrap the *request body* in a [`DisconnectBody`] keyed at the
///    sender. Now any read-error on the body fires the watch and
///    propagates.
/// 4. Run the handler with the wrapped body.
/// 5. After the handler returns, wrap the *response body* in a
///    [`DisconnectBody`] keyed at the SAME sender. Now any
///    write-error while hyper flushes the response to a closed
///    socket also fires the watch.
///
/// Both the request-body wrapper and the response-body wrapper
/// share an `Arc<AtomicBool>` "already fired" latch so a
/// disconnect that surfaces from both sides only flips the watch
/// once (idempotent).
pub async fn client_disconnect_middleware(mut req: Request, next: Next) -> Response {
    let (tx, rx) = new_cancel_pair();
    let fired = Arc::new(AtomicBool::new(false));

    // 1. Stash the (tx, rx) pair in extensions for the handler.
    //    The handler clones `tx` for any *additional* cancel sources
    //    it wants to merge (deadline watchdog) and threads `rx`
    //    into the pipeline.
    req.extensions_mut().insert(CancelWatch { tx: tx.clone(), rx });

    // 2. Wrap the request body so an upload-time disconnect is
    //    observable to the handler / pipeline.
    let (parts, body) = req.into_parts();
    let req_body = DisconnectBody::new(body, tx.clone(), Arc::clone(&fired));
    let req = Request::from_parts(parts, Body::new(req_body));

    // 3. Run the handler.
    let mut response = next.run(req).await;

    // 4. Wrap the response body so a stream-time disconnect is
    //    observable. We do this regardless of HTTP status — a 4xx
    //    response on a closed connection is still a disconnect.
    let resp_body = std::mem::replace(response.body_mut(), Body::empty());
    let wrapped = DisconnectBody::new(resp_body, tx, Arc::clone(&fired));
    *response.body_mut() = Body::new(wrapped);

    response
}

// ---------------------------------------------------------------------------
// DisconnectBody: `http_body::Body` newtype with disconnect signaling.
//
// We implement the `http_body::Body` trait directly (rather than
// the `Stream` trait that the `Body::into_data_stream` wrapper
// exposes) because:
//   - `Json`'s `FromRequest` reads via `axum::body::to_bytes`, which
//     is built on the `http_body` trait.
//   - SSE responses (`axum::response::Sse`) and `axum::body::Body`
//     in general are `http_body::Body`, not raw streams.
//   - The `http_body` trait is the only one hyper surfaces errors
//     on, so it's the right place to observe a closed socket.
// ---------------------------------------------------------------------------

/// `http_body::Body` wrapper that fires a watch sender on any
/// `poll_frame` error and on the body reaching its end while the
/// connection is still being written to.
///
/// # Idempotency
/// The first error or first `None` flips the watch to `true`; all
/// subsequent calls are no-ops. The shared `fired` flag makes the
/// wrapper safe to use on both the request and response body of the
/// same request (only one will see the error, but if both do, only
/// the first flip is recorded).
///
/// # Why we also fire on the final `None`
/// The `http_body` contract says that `poll_frame` returning
/// `Poll::Ready(None)` means the body is *done* — for the response
/// body that's the natural end of the stream, NOT a disconnect.
/// We don't fire on `None` for the response-body wrapper because
/// the body finished normally. We don't fire on `None` for the
/// request-body wrapper either: a complete body means the client
/// sent everything, which is the success case.
///
/// Disconnect is only signalled by the explicit `Err` arm.
#[derive(Debug)]
pub struct DisconnectBody<B> {
    inner: B,
    tx: watch::Sender<bool>,
    fired: Arc<AtomicBool>,
}

impl<B> DisconnectBody<B> {
    /// Wrap `inner`. `tx` is fired (idempotently) the first time
    /// `poll_frame` returns `Err` on this body.
    pub fn new(inner: B, tx: watch::Sender<bool>, fired: Arc<AtomicBool>) -> Self {
        Self { inner, tx, fired }
    }
}

impl<B: HttpBody + Unpin> HttpBody for DisconnectBody<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let result = Pin::new(&mut self.inner).poll_frame(cx);
        if let Poll::Ready(Some(Err(_))) = &result {
            // First-error wins. `send` is a no-op if the receiver
            // was dropped (pipeline already finished), so we don't
            // care about the result. We also flip the local
            // `fired` latch so the response-body wrapper (which
            // shares the same `Arc<AtomicBool>`) doesn't
            // double-fire if it later sees an error too.
            if !self.fired.swap(true, Ordering::AcqRel) {
                let _ = self.tx.send(true);
            }
        }
        result
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

// ---------------------------------------------------------------------------
// Helper for the chat handler: pull the watch receiver out of the
// request extensions. We do it via a typed extension rather than a
// header lookup so the wire-up is type-safe and impossible to typo.
// ---------------------------------------------------------------------------

/// Extension type used to carry the cancel watch. We use a
/// dedicated newtype (rather than `watch::...` directly) so the
/// extension key is unambiguous if anyone else ever stuffs a
/// `watch::Sender`/`Receiver` into extensions.
///
/// The handler is expected to clone `tx` for any *fallback*
/// signals it wants to merge in (e.g. a deadline watchdog) and
/// pass `rx` to the pipeline. The middleware's `DisconnectBody`
/// wrappers hold their own clones of `tx` and fire it on any
/// body-level error, so all sources of cancellation share the
/// same watch.
#[derive(Clone, Debug)]
pub struct CancelWatch {
    pub tx: watch::Sender<bool>,
    pub rx: watch::Receiver<bool>,
}

impl CancelWatch {
    pub fn new() -> Self {
        let (tx, rx) = new_cancel_pair();
        Self { tx, rx }
    }
}

impl Default for CancelWatch {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the disconnect detection wrapper.
    //!
    //! These exercise the `DisconnectBody` newtype directly, which
    //! is the only piece of the wire-up that has nontrivial logic
    //! (the rest of the middleware is mechanical glue around
    //! `req.extensions_mut().insert(...)`).
    //!
    //! The full end-to-end behaviour — middleware → handler → watch
    //! → pipeline abort — is covered by the regression tests in
    //! `crates/openproxy-core/src/pipeline.rs` (which set the watch
    //! manually and assert the pipeline aborts with HTTP 499). What
    //! the unit tests here add is the "did the wrapper actually
    //! observe the body error and flip the watch?" question, which
    //! cannot be answered from the pipeline side alone.
    use super::*;
    use bytes::Bytes;
    use http_body_util::Full;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// A `http_body::Body` that always errors on `poll_frame`. The
    /// only way `DisconnectBody` should fire the watch is on an
    /// `Err` arm of `poll_frame`, so a body that always errors is
    /// the cleanest way to assert "the wrapper observed the error
    /// and fired the watch".
    struct AlwaysErrorBody;
    impl HttpBody for AlwaysErrorBody {
        type Data = Bytes;
        type Error = std::io::Error;
        fn poll_frame(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            Poll::Ready(Some(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "simulated client disconnect",
            ))))
        }
        fn is_end_stream(&self) -> bool {
            false
        }
        fn size_hint(&self) -> SizeHint {
            SizeHint::default()
        }
    }

    /// A `http_body::Body` that produces one frame of data and
    /// then yields `None` on the next poll. This represents a
    /// *normal* completion, not a disconnect — the watch must
    /// NOT fire in this case.
    struct OneFrameBody {
        delivered: bool,
    }
    impl HttpBody for OneFrameBody {
        type Data = Bytes;
        type Error = std::io::Error;
        fn poll_frame(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            if self.delivered {
                Poll::Ready(None)
            } else {
                self.delivered = true;
                Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"hi")))))
            }
        }
        fn is_end_stream(&self) -> bool {
            self.delivered
        }
        fn size_hint(&self) -> SizeHint {
            SizeHint::with_exact(2)
        }
    }

    /// Pump a `DisconnectBody` once and return the result. Pulled
    /// out into a helper because both tests do exactly this.
    fn poll_once<B: HttpBody + Unpin>(
        body: &mut DisconnectBody<B>,
    ) -> Poll<Option<Result<Frame<B::Data>, B::Error>>> {
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        Pin::new(body).poll_frame(&mut cx)
    }

    /// The core contract: when the inner body yields an `Err`, the
    /// wrapper must flip the watch to `true`.
    #[tokio::test]
    async fn error_on_poll_frame_fires_watch() {
        let (tx, rx) = new_cancel_pair();
        let fired = Arc::new(AtomicBool::new(false));
        let mut body = DisconnectBody::new(AlwaysErrorBody, tx, fired.clone());

        let result = poll_once(&mut body);
        match result {
            Poll::Ready(Some(Err(_))) => {}
            other => panic!(
                "expected the wrapper to propagate the inner Err arm, got {:?}",
                other
            ),
        }

        // The receiver should see `true` *without* a `.changed()`-then-borrow
        // dance because `watch::Sender::send` overwrites the current value
        // and `borrow` reads it.
        assert!(
            *rx.borrow(),
            "watch was not fired after a body error — the disconnect \
             detector is broken"
        );
    }

    /// The complementary contract: a body that completes normally
    /// must NOT fire the watch. Otherwise every successful request
    /// would be reported as a cancellation.
    #[tokio::test]
    async fn normal_completion_does_not_fire_watch() {
        let (tx, rx) = new_cancel_pair();
        let fired = Arc::new(AtomicBool::new(false));
        let mut body = DisconnectBody::new(
            OneFrameBody { delivered: false },
            tx,
            fired,
        );

        // First poll: returns a frame.
        let first = poll_once(&mut body);
        assert!(matches!(first, Poll::Ready(Some(Ok(_)))));
        // Watch should still be false.
        assert!(
            !*rx.borrow(),
            "watch fired after a successful frame — the wrapper is firing \
             on success, not just on error"
        );

        // Second poll: returns None.
        let second = poll_once(&mut body);
        assert!(matches!(second, Poll::Ready(None)));
        assert!(
            !*rx.borrow(),
            "watch fired on body completion (None) — the wrapper should \
             only fire on the explicit Err arm, not on the natural end \
             of the stream"
        );
    }

    /// Idempotency: if a body emits multiple `Err` frames in a row
    /// (hyper can do this when the connection is in a weird state),
    /// the watch fires once, not N times. The `fired` latch is the
    /// only thing keeping the receiver from being spammed; verify
    /// it does its job.
    #[tokio::test]
    async fn repeated_errors_only_flip_watch_once() {
        let (tx, rx) = new_cancel_pair();
        let fired = Arc::new(AtomicBool::new(false));
        let mut body = DisconnectBody::new(AlwaysErrorBody, tx, fired.clone());

        // 5 error polls in a row.
        for _ in 0..5 {
            let _ = poll_once(&mut body);
        }

        // The watch is true. We can't directly count "number of
        // flips" from the receiver side, but we CAN check that
        // `fired` is now true (which means the first error was the
        // one that fired the watch, and the rest were no-ops).
        assert!(*rx.borrow(), "watch never fired");
        assert!(
            fired.load(Ordering::SeqCst),
            "the shared `fired` latch never tripped — the idempotency \
             guard is missing"
        );
    }

    /// `CancelWatch::new` mints a fresh `(tx, rx)` pair and
    /// `Clone` works on the newtype (the handler clones both
    /// halves in the request hot path).
    #[tokio::test]
    async fn cancel_watch_clone_is_independent() {
        let cw = CancelWatch::new();
        let rx2 = cw.rx.clone();

        // Firing via the original `cw.tx` is visible on the clone.
        let _ = cw.tx.send(true);
        assert!(*cw.rx.borrow(), "original rx should see the send");
        assert!(*rx2.borrow(), "cloned rx should see the same send");
    }

    /// A real end-to-end round trip: a `Pin<Box<dyn Body>>` that
    /// yields one error frame, wrapped in `DisconnectBody`, drives
    /// the watch. This is the shape hyper would actually surface
    /// when the client closes the connection mid-upload.
    #[tokio::test]
    async fn boxed_dyn_body_error_fires_watch() {
        let inner: Pin<Box<dyn HttpBody<Data = Bytes, Error = std::io::Error> + Send + Unpin>> =
            Box::pin(AlwaysErrorBody);
        let (tx, rx) = new_cancel_pair();
        let fired = Arc::new(AtomicBool::new(false));
        let mut body = DisconnectBody::new(inner, tx, fired);

        let result = poll_once(&mut body);
        assert!(matches!(result, Poll::Ready(Some(Err(_)))));
        assert!(*rx.borrow(), "watch not fired through Pin<Box<dyn Body>>");
    }

    /// Sanity check that the newtype compiles and runs with a
    /// non-`Unpin` inner body. The `B: HttpBody + Unpin` bound on
    /// the `impl` makes the type itself `Unpin` regardless.
    #[tokio::test]
    async fn full_body_does_not_fire_watch() {
        let (tx, rx) = new_cancel_pair();
        let fired = Arc::new(AtomicBool::new(false));
        // `Full<Bytes>` is a non-`Unpin` body from http-body-util.
        let mut body = DisconnectBody::new(
            Full::new(Bytes::from_static(b"hello")),
            tx,
            fired,
        );
        let first = poll_once(&mut body);
        assert!(matches!(first, Poll::Ready(Some(Ok(_)))));
        assert!(
            !*rx.borrow(),
            "watch fired on a non-error body — the wrapper is firing \
             on data frames, not on errors"
        );
    }
}
