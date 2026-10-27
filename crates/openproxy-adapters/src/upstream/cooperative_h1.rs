//! Direct HTTP/1 transport (default for plain `http://` upstreams). Headers
//! and body drive hyper's connection on the consuming task, never on a spawned
//! driver. HTTPS, proxies and protocol upgrades stay on the legacy transport.
//!
//! Keep-alive without background tasks: a response owns its connection while
//! the body is read. A body that reached EOF hands the connection back to an
//! idle pool where nothing drives it. A peer close is detected at checkout, and
//! a request the stale connection never sent is retried once on a fresh one.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::uri::Authority;
use http::{Request, Response, Uri};
use http_body::Body;
use hyper::body::{Frame, Incoming};
use hyper::client::conn::http1::SendRequest;
use hyper_util::client::legacy::connect::Connection as _;
use parking_lot::Mutex;
use tower_service::Service;

use super::client::{ProductionTransport, UpstreamTransport, format_hyper_error};
use super::connector::{CALL_PROXY, CALL_TIMEOUTS, PhasedConnector, PhasedTimeouts};
use super::error::{UpstreamError, UpstreamResult};
use super::phases::UpstreamPhase;
use super::request_body::RequestBody;
use super::response::TransportBody;
use super::upload::{ConnectionProgressSender, FlushProgress, ObservedConnector, ObservedIo};

type H1Connection = hyper::client::conn::http1::Connection<
    ObservedIo<super::connector_types::PhasedConnection>,
    RequestBody,
>;

/// Same limits as the legacy client pool.
const MAX_IDLE_PER_HOST: usize = 8;
const IDLE_TIMEOUT: Duration = Duration::from_secs(20);
/// Frames read without a waker when a body is dropped before EOF. Recovers the
/// connection when the terminator already arrived (`[DONE]` then `0\r\n\r\n`),
/// and closes it when the upstream is still generating.
const DRAIN_POLLS: usize = 16;

#[derive(Debug)]
pub(crate) struct CooperativeH1Transport {
    connector: PhasedConnector,
    idle: Arc<IdlePool>,
    fallback: ProductionTransport,
}

impl CooperativeH1Transport {
    pub(crate) fn new() -> Self {
        Self {
            connector: PhasedConnector::with_defaults(),
            idle: Arc::new(IdlePool::default()),
            fallback: ProductionTransport::new(),
        }
    }

    async fn connect(&self, uri: &Uri, timeouts: PhasedTimeouts) -> UpstreamResult<Idle> {
        let mut connector = ObservedConnector(self.connector.clone());
        let io = CALL_PROXY
            .scope(
                None,
                CALL_TIMEOUTS.scope(timeouts, connector.call(uri.clone())),
            )
            .await
            .map_err(|error| {
                if let Some(phased) = error.downcast_ref::<super::connector::PhasedConnectorError>()
                {
                    super::client::map_phased_connector_error(phased)
                } else {
                    UpstreamError::Connection(error.to_string())
                }
            })?;
        // CaptureConnection's publisher is private to hyper-util's legacy
        // client. The flush observer travels with the connection instead.
        let mut extras = http::Extensions::new();
        io.connected().get_extras(&mut extras);
        let flush = extras
            .remove::<FlushProgress>()
            .ok_or_else(|| UpstreamError::Http("missing flush observer".into()))?;
        let (sender, connection) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|error| UpstreamError::Http(format_hyper_error(&error)))?;
        Ok(Idle {
            sender,
            connection,
            flush,
            since: Instant::now(),
        })
    }
}

impl UpstreamTransport for CooperativeH1Transport {
    fn uses_cooperative_connection(&self, req: &Request<RequestBody>, proxy: Option<&str>) -> bool {
        req.uri().scheme_str() == Some("http")
            && proxy.is_none()
            && matches!(
                req.version(),
                http::Version::HTTP_10 | http::Version::HTTP_11
            )
            && req.method() != http::Method::CONNECT
            && !req.headers().contains_key(http::header::UPGRADE)
    }

    fn send_request(
        &self,
        mut req: Request<RequestBody>,
        connector_timeouts: PhasedTimeouts,
        proxy_url: Option<String>,
    ) -> Pin<Box<dyn Future<Output = UpstreamResult<Response<TransportBody>>> + Send + '_>> {
        if !self.uses_cooperative_connection(&req, proxy_url.as_deref()) {
            return self
                .fallback
                .send_request(req, connector_timeouts, proxy_url);
        }
        Box::pin(async move {
            let uri = req.uri().clone();
            let authority = uri
                .authority()
                .cloned()
                .ok_or_else(|| UpstreamError::Invalid("missing authority".into()))?;
            let progress = req
                .extensions_mut()
                .remove::<ConnectionProgressSender>()
                .ok_or_else(|| UpstreamError::Http("missing connection observer".into()))?;
            if !req.headers().contains_key(http::header::HOST) {
                let host = http::HeaderValue::from_str(authority.as_str())
                    .map_err(|error| UpstreamError::Invalid(error.to_string()))?;
                req.headers_mut().insert(http::header::HOST, host);
            }
            // Origin-form. `http::Uri` normalises a missing path to "/".
            *req.uri_mut() = uri
                .path_and_query()
                .map_or("/", |pq| pq.as_str())
                .parse()
                .map_err(|error: http::uri::InvalidUri| {
                    UpstreamError::Invalid(error.to_string())
                })?;

            let mut pooled = self.idle.checkout(&authority);
            loop {
                let reused = pooled.is_some();
                let mut conn = match pooled.take() {
                    Some(idle) => idle,
                    None => self.connect(&uri, connector_timeouts).await?,
                };
                progress.0.send_replace(Some(conn.flush.clone()));
                match send_on(&mut conn, req).await {
                    Sent::Response(response, finished) => {
                        let (parts, incoming) = response.into_parts();
                        let Idle {
                            sender,
                            connection,
                            flush,
                            ..
                        } = conn;
                        let open = !finished;
                        return Ok(Response::from_parts(
                            parts,
                            TransportBody::Cooperative(Box::new(CooperativeIncoming {
                                incoming,
                                connection: open.then_some(connection),
                                error: None,
                                eof: false,
                                release: open.then(|| Release {
                                    pool: Arc::clone(&self.idle),
                                    authority: authority.clone(),
                                    sender,
                                    flush,
                                }),
                            })),
                        ));
                    }
                    // A stale idle connection rejected the request before
                    // writing it: replaying it cannot duplicate side effects.
                    Sent::Unsent(request, _) if reused => req = request,
                    Sent::Unsent(_, error) | Sent::Failed(error) => {
                        return Err(UpstreamError::Http(error));
                    }
                }
            }
        })
    }

    fn phase_hint(&self) -> Option<UpstreamPhase> {
        None
    }
}

enum Sent {
    /// `true` when the connection already finished with the headers.
    Response(Response<Incoming>, bool),
    Unsent(Request<RequestBody>, String),
    Failed(String),
}

/// Drive the request and its connection on the calling task until headers.
async fn send_on(conn: &mut Idle, req: Request<RequestBody>) -> Sent {
    let mut sending = Box::pin(conn.sender.try_send_request(req));
    let mut finished = false;
    let mut closed: Option<String> = None;
    futures_util::future::poll_fn(|cx| {
        if !finished && let Poll::Ready(result) = Pin::new(&mut conn.connection).poll(cx) {
            finished = true;
            closed = result.err().map(|error| format_hyper_error(&error));
        }
        // A finished connection resolves every queued or in-flight request,
        // so this cannot stay pending once `finished` is set.
        match sending.as_mut().poll(cx) {
            Poll::Ready(Ok(response)) => Poll::Ready(Sent::Response(response, finished)),
            Poll::Ready(Err(mut error)) => {
                let request = error.take_message();
                let message = closed
                    .take()
                    .unwrap_or_else(|| format_hyper_error(error.error()));
                Poll::Ready(match request {
                    Some(request) => Sent::Unsent(request, message),
                    None => Sent::Failed(message),
                })
            }
            Poll::Pending if finished => Poll::Ready(Sent::Failed(
                closed
                    .take()
                    .unwrap_or_else(|| "connection closed before response".into()),
            )),
            Poll::Pending => Poll::Pending,
        }
    })
    .await
}

struct Idle {
    sender: SendRequest<RequestBody>,
    connection: H1Connection,
    flush: FlushProgress,
    since: Instant,
}

impl Idle {
    /// One poll without a waker: a peer close or leftover bytes surface as
    /// `Ready`. Nothing drives an idle connection, so a stale `Pending`
    /// registration is replaced by the next real poll.
    fn reusable(&mut self) -> bool {
        let mut cx = Context::from_waker(Waker::noop());
        self.since.elapsed() < IDLE_TIMEOUT
            && Pin::new(&mut self.connection).poll(&mut cx).is_pending()
            && self.sender.is_ready()
    }
}

#[derive(Default)]
struct IdlePool(Mutex<HashMap<Authority, Vec<Idle>>>);

impl std::fmt::Debug for IdlePool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let idle = self.0.lock().values().map(Vec::len).sum::<usize>();
        f.debug_struct("IdlePool").field("idle", &idle).finish()
    }
}

impl IdlePool {
    /// Most recently used first. Validation runs outside the lock.
    fn checkout(&self, authority: &Authority) -> Option<Idle> {
        loop {
            let mut idle = {
                let mut hosts = self.0.lock();
                let entries = hosts.get_mut(authority)?;
                let idle = entries.pop();
                if entries.is_empty() {
                    hosts.remove(authority);
                }
                idle?
            };
            if idle.reusable() {
                return Some(idle);
            }
        }
    }

    fn put(&self, authority: Authority, mut idle: Idle) {
        if !idle.reusable() {
            return;
        }
        let mut hosts = self.0.lock();
        // No reaper task: expired entries are swept on every return.
        hosts.retain(|_, entries| {
            entries.retain(|entry| entry.since.elapsed() < IDLE_TIMEOUT);
            !entries.is_empty()
        });
        let entries = hosts.entry(authority).or_default();
        if entries.len() < MAX_IDLE_PER_HOST {
            entries.push(idle);
        }
    }
}

/// What a body needs to hand its connection back after EOF.
struct Release {
    pool: Arc<IdlePool>,
    authority: Authority,
    sender: SendRequest<RequestBody>,
    flush: FlushProgress,
}

/// Exclusive ownership needs neither Arc nor Mutex nor a spawned driver.
/// A pending body poll performs at most one connection poll, then rechecks the
/// body. Both I/O and body readiness register the real consuming task's waker.
pub(crate) struct CooperativeIncoming {
    incoming: Incoming,
    connection: Option<H1Connection>,
    error: Option<hyper::Error>,
    eof: bool,
    release: Option<Release>,
}

impl Body for CooperativeIncoming {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        if let Poll::Ready(frame) = Pin::new(&mut self.incoming).poll_frame(cx) {
            self.eof |= frame.is_none();
            return Poll::Ready(frame);
        }
        if let Some(connection) = self.connection.as_mut()
            && let Poll::Ready(result) = Pin::new(connection).poll(cx)
        {
            self.connection = None;
            self.error = result.err();
        }
        let frame = Pin::new(&mut self.incoming).poll_frame(cx);
        if frame.is_pending()
            && let Some(error) = self.error.take()
        {
            return Poll::Ready(Some(Err(error)));
        }
        if let Poll::Ready(None) = frame {
            self.eof = true;
        }
        frame
    }

    fn is_end_stream(&self) -> bool {
        self.incoming.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.incoming.size_hint()
    }
}

impl CooperativeIncoming {
    /// Read frames that already arrived, without waiting for more.
    fn drain_ready(&mut self) {
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..DRAIN_POLLS {
            match Pin::new(&mut *self).poll_frame(&mut cx) {
                Poll::Ready(Some(Ok(_))) => {}
                Poll::Ready(Some(Err(_)) | None) | Poll::Pending => return,
            }
        }
    }
}

impl Drop for CooperativeIncoming {
    fn drop(&mut self) {
        if self.release.is_none() {
            return;
        }
        if !self.eof {
            self.drain_ready();
        }
        if let (true, Some(release), Some(connection)) =
            (self.eof, self.release.take(), self.connection.take())
        {
            release.pool.put(
                release.authority,
                Idle {
                    sender: release.sender,
                    connection,
                    flush: release.flush,
                    since: Instant::now(),
                },
            );
        }
    }
}

#[cfg(test)]
#[path = "cooperative_h1_tests.rs"]
mod tests;
