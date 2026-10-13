//! Bounded upload frames with per-request progress, never per-connection state.
//!
//! Body consumption plus a subsequent successful connection flush define
//! upload completion. This is not an acknowledgement from the server.
//! Bounded frames preserve backpressure for large payloads.
//! Generation is governed separately by TTFT (stream) or total (unary).
use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use tokio::sync::watch;

#[derive(Debug)]
pub struct RequestBody {
    bytes: Bytes,
    completed: watch::Sender<Option<Instant>>,
}

impl RequestBody {
    pub(crate) fn new(bytes: Bytes) -> (Self, watch::Receiver<Option<Instant>>) {
        let (completed, progress) = watch::channel(if bytes.is_empty() {
            Some(Instant::now())
        } else {
            None
        });
        (Self { bytes, completed }, progress)
    }
}

impl Drop for RequestBody {
    fn drop(&mut self) {
        // HTTP/1 knows Content-Length and drops the body after its last frame
        // without polling EOF. HTTP/2 retains it until the send task finishes.
        if self.bytes.is_empty() && self.completed.borrow().is_none() {
            self.completed.send_replace(Some(Instant::now()));
        }
    }
}

impl Body for RequestBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        if self.bytes.is_empty() {
            if self.completed.borrow().is_none() {
                self.completed.send_replace(Some(Instant::now()));
            }
            Poll::Ready(None)
        } else {
            let len = self.bytes.len().min(16 * 1024);
            let frame = Frame::data(self.bytes.split_to(len));
            Poll::Ready(Some(Ok(frame)))
        }
    }

    fn is_end_stream(&self) -> bool {
        // Hyper must poll EOF even for empty bodies, so completion is observable.
        self.completed.borrow().is_some()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.bytes.len() as u64)
    }
}
