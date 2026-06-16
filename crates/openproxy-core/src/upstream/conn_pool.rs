//! Connection pool keyed by `(scheme, host, port)`.
//!
//! The spec calls for `Mutex<HashMap<HostKey, hyper::client::conn::http1::SendRequest>>`.
//! We deviate from that exact primitive (see "Deviations from the spec"
//! in the module-level docs) because `SendRequest` is not `Clone` and
//! owns its half of the connection: holding a `SendRequest` inside a
//! shared `Mutex` would require `&mut` access for every send, which
//! forces a global write lock per request and serializes all traffic
//! to the same host.
//!
//! The "right primitive" in hyper 1.10 is `hyper_util::client::legacy::Client`,
//! which is `Clone` and shares an internal per-host pool. We keep the
//! spec's surface (`UpstreamConnectionPool` is `Clone`, exposes a
//! `reuses()` counter) and use the legacy `Client` underneath.
//!
//! For unit-test observability, a `PoolObserver` connector is supported:
//! when used, the pool tracks how many times a connection was reused
//! (a borrowed-already-open connection from the pool) vs. freshly
//! dialed. The counter is exposed via `UpstreamConnectionPool::reuses()`
//! and is what the `conn_pool_reuse` test asserts on.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::phases::UpstreamPhase;

/// `scheme://host:port` tuple that keys a pooled connection.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct HostKey {
    pub scheme: Scheme,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    pub fn from_uri(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "https" => Scheme::Https,
            _ => Scheme::Http,
        }
    }
}

impl HostKey {
    pub fn new(scheme: Scheme, host: impl Into<String>, port: u16) -> Self {
        Self { scheme, host: host.into(), port }
    }
}

/// Per-host, lazily-initialized pool entry.
///
/// In the current implementation the actual connection reuse is done
/// by `hyper_util::client::legacy::Client`'s internal pool. This struct
/// is the user-facing wrapper: it tracks the per-key "is the connection
/// warm?" hint and the observability counter.
#[derive(Debug, Default)]
struct PoolEntry {
    /// Number of times a request to this host reused an already-open
    /// connection (i.e. the second and later requests in a burst).
    /// The first request is a "dial", not a "reuse".
    reuses: AtomicUsize,
    /// Total requests to this host. Useful as a denominator.
    total: AtomicUsize,
    /// Last successful use as a unix-ish counter (monotonic). Used by
    /// the eviction sweep.
    last_used_tick: AtomicUsize,
}

impl PoolEntry {
    fn new(tick: usize) -> Self {
        Self {
            reuses: AtomicUsize::new(0),
            total: AtomicUsize::new(0),
            last_used_tick: AtomicUsize::new(tick),
        }
    }
}

/// A shared, observable handle to a per-host connection pool.
///
/// Cloning is cheap (one `Arc` clone). The actual pooled connections
/// live in `hyper_util::client::legacy::Client`; this struct holds
/// the observability counters and the idle-eviction tick generator.
///
/// Idle eviction: every clone shares a single `Mutex<HashMap<HostKey,
/// PoolEntry>>`. A background sweep (started by `UpstreamClient::new`)
/// wakes up every 30s and drops any entry whose `last_used_tick` is
/// more than 60s old in tick-space. Because the underlying
/// `hyper_util::client::legacy::Client` owns the real sockets, the
/// sweep only affects the observability map; the legacy client will
/// re-dial on the next request to that host.
#[derive(Clone, Default)]
pub struct UpstreamConnectionPool {
    inner: Arc<Mutex<HashMap<HostKey, PoolEntry>>>,
    /// Monotonically increasing tick. Bumped by the background sweep.
    tick: Arc<AtomicUsize>,
}

impl UpstreamConnectionPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Total reuses across all hosts. Used by the `conn_pool_reuse`
    /// unit test.
    pub fn reuses(&self) -> usize {
        let g = self.inner.lock().expect("pool mutex poisoned");
        g.values().map(|e| e.reuses.load(Ordering::SeqCst)).sum()
    }

    /// Total requests across all hosts.
    pub fn total(&self) -> usize {
        let g = self.inner.lock().expect("pool mutex poisoned");
        g.values().map(|e| e.total.load(Ordering::SeqCst)).sum()
    }

    /// Number of distinct hosts that have been seen at least once.
    pub fn host_count(&self) -> usize {
        self.inner.lock().expect("pool mutex poisoned").len()
    }

    /// Per-host reuses (for debugging / tests).
    pub fn reuses_for(&self, key: &HostKey) -> usize {
        self.inner
            .lock()
            .expect("pool mutex poisoned")
            .get(key)
            .map(|e| e.reuses.load(Ordering::SeqCst))
            .unwrap_or(0)
    }

    /// Record that a request to `key` just used a freshly-dialed
    /// connection (i.e. it was the first request in a burst).
    pub fn record_dial(&self, key: HostKey) {
        let mut g = self.inner.lock().expect("pool mutex poisoned");
        let tick = self.tick.fetch_add(1, Ordering::SeqCst);
        let entry = g.entry(key).or_insert_with(|| PoolEntry::new(tick));
        entry.total.fetch_add(1, Ordering::SeqCst);
        entry.last_used_tick.store(tick, Ordering::SeqCst);
    }

    /// Record that a request to `key` just reused a pooled connection.
    pub fn record_reuse(&self, key: HostKey) {
        let mut g = self.inner.lock().expect("pool mutex poisoned");
        let tick = self.tick.fetch_add(1, Ordering::SeqCst);
        let entry = g.entry(key).or_insert_with(|| PoolEntry::new(tick));
        entry.total.fetch_add(1, Ordering::SeqCst);
        entry.reuses.fetch_add(1, Ordering::SeqCst);
        entry.last_used_tick.store(tick, Ordering::SeqCst);
    }

    /// Drop entries older than `max_tick_age` ticks. Called by the
    /// background sweep. Returns the number of entries evicted.
    pub fn evict_older_than(&self, max_tick_age: usize) -> usize {
        let mut g = self.inner.lock().expect("pool mutex poisoned");
        let current = self.tick.load(Ordering::SeqCst);
        let cutoff = current.saturating_sub(max_tick_age);
        let before = g.len();
        g.retain(|_, e| e.last_used_tick.load(Ordering::SeqCst) >= cutoff);
        before - g.len()
    }
}

impl std::fmt::Debug for UpstreamConnectionPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let g = self.inner.lock().expect("pool mutex poisoned");
        f.debug_struct("UpstreamConnectionPool")
            .field("host_count", &g.len())
            .field("reuses", &g.values().map(|e| e.reuses.load(Ordering::SeqCst)).sum::<usize>())
            .field("total", &g.values().map(|e| e.total.load(Ordering::SeqCst)).sum::<usize>())
            .finish()
    }
}

/// Phase-aware connector wrapper.
///
/// The spec calls out a `phase_timeout_dns` and `phase_timeout_tls`
/// test, which means the connector must surface which phase is
/// stalling. The standard `HttpConnector` / `HttpsConnector` report a
/// single `Box<dyn Error>` per connection attempt. To get a
/// phase-attributed timeout we wrap the underlying I/O into a custom
/// `tower::Service<Uri>` that reports failures through the `phase`
/// channel. This is the connector shape used by every test in
/// `tests.rs`.
///
/// When the `upstream-hyper` feature is on, `phase_for_uri` is used
/// to label each phase. When off, the connector is unused and the
/// client returns `UpstreamError::Invalid` for every call.
#[cfg(feature = "upstream-hyper")]
pub mod connector {
    use super::super::phases::UpstreamPhase;
    use super::Scheme;
    use bytes::Bytes;
    use http::Uri;
    use hyper::body::Body;
    use hyper_util::client::legacy::connect::{Connection as HyperConnection, HttpConnector};
    use hyper_util::rt::TokioIo;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use tokio::net::TcpStream;
    use tower_service::Service;

    // The `PhaseReportingConnector` trait was speculatively added
    // here but isn't used; the test connectors implement
    // `tower_service::Service<Uri>` directly and the
    // `UpstreamClient::for_test_with_connector` takes a generic
    // over the connector type with no separate trait.

    /// The default connector: HTTP via `HttpConnector`. HTTPS is a
    /// follow-up gate (the `HttpsConnector` from `hyper-rustls` is
    /// already in the lockfile and will be wired in via a builder
    /// in Gate 1+). For Gate 0 the production path handles HTTP
    /// only; HTTPS is supported in unit tests via a custom
    /// test connector.
    ///
    /// Phase reporting is done at the `UpstreamClient::call` level
    /// (it wraps the connect future in a per-phase timer).
    #[derive(Clone)]
    pub struct DefaultConnector {
        http: HttpConnector,
    }

    impl DefaultConnector {
        pub fn new() -> Self {
            let mut http = HttpConnector::new();
            http.enforce_http(false);
            http.set_nodelay(true);
            Self { http }
        }
    }

    impl Default for DefaultConnector {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Service<Uri> for DefaultConnector {
        // The hyper-util `Connect` blanket impl requires the
        // response type to be a CONCRETE `Read + Write +
        // Connection + Unpin + Send + 'static` (not a trait
        // object). We therefore return `TokioIo<TcpStream>` for
        // HTTP and a boxed error for HTTPS.
        type Response = TokioIo<TcpStream>;
        type Error = Box<dyn std::error::Error + Send + Sync>;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.http.poll_ready(cx).map_err(Into::into)
        }

        fn call(&mut self, dst: Uri) -> Self::Future {
            // We only support HTTP in the Gate 0 production
            // connector. HTTPS requests will fail with an error
            // here; the unit tests that need HTTPS use a custom
            // connector via `for_test_with_connector`.
            if dst.scheme_str() == Some("https") {
                return Box::pin(async move {
                    Err(Box::new(std::io::Error::other(
                        "https not yet supported by DefaultConnector in Gate 0",
                    ))
                        as Box<dyn std::error::Error + Send + Sync>)
                });
            }
            let http_fut = self.http.call(dst);
            Box::pin(async move {
                let io = http_fut
                    .await
                    .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                        Box::new(std::io::Error::other(e.to_string()))
                    })?;
                // `HttpConnector` already returns
                // `TokioIo<TcpStream>`, which implements
                // `hyper::rt::Read`/`Write` and the `Connection`
                // marker trait.
                Ok(io)
            })
        }
    }
}
