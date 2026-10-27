//! Observe successful socket flushes, including pooled connections and TLS.
use hyper::rt::{Read, Write};
use hyper_util::client::legacy::connect::{Connected, Connection};
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;
use tokio::sync::watch;

#[derive(Clone, Debug)]
pub(crate) struct FlushProgress(pub watch::Receiver<Option<Instant>>);

/// Connection publication for a direct hyper driver (legacy capture is private).
#[derive(Clone, Debug)]
pub(crate) struct ConnectionProgressSender(pub watch::Sender<Option<FlushProgress>>);

/// Upload-body completion plus connection-flush publication for either driver.
pub(crate) type DispatchProgress = (
    hyper_util::client::legacy::connect::CaptureConnection,
    watch::Receiver<Option<Instant>>,
    Option<watch::Receiver<Option<FlushProgress>>>,
);

pub(crate) struct ObservedIo<T> {
    inner: T,
    flushed: watch::Sender<Option<Instant>>,
}
impl<T> ObservedIo<T> {
    pub(crate) fn new(inner: T) -> Self {
        let (flushed, _) = watch::channel(None);
        Self { inner, flushed }
    }
}
impl<T: Connection> Connection for ObservedIo<T> {
    fn connected(&self) -> Connected {
        self.inner
            .connected()
            .extra(FlushProgress(self.flushed.subscribe()))
    }
}
impl<T: Read + Unpin> Read for ObservedIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}
impl<T: Write + Unpin> Write for ObservedIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let result = Pin::new(&mut self.inner).poll_flush(cx);
        if matches!(result, Poll::Ready(Ok(()))) {
            self.flushed.send_replace(Some(Instant::now()));
        }
        result
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }
}

#[derive(Clone)]
pub(crate) struct ObservedConnector<C>(pub C);
impl<C: tower_service::Service<http::Uri> + Send, T> tower_service::Service<http::Uri>
    for ObservedConnector<C>
where
    C: tower_service::Service<http::Uri, Response = T>,
    C::Future: Send + 'static,
    T: Send + 'static,
{
    type Response = ObservedIo<T>;
    type Error = C::Error;
    type Future =
        Pin<Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>>;
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }
    fn call(&mut self, uri: http::Uri) -> Self::Future {
        let future = self.0.call(uri);
        Box::pin(async move { future.await.map(ObservedIo::new) })
    }
}
