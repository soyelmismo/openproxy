//! Stub types for builds with `upstream-hyper` disabled.
//!
//! These types exist so `crate::upstream::UpstreamClient`,
//! `crate::upstream::TimeoutProfile`, etc. resolve at compile time
//! even when the feature is off. They are NOT functional: constructing
//! a `StubClient` is fine, but `call()` returns
//! `UpstreamError::Invalid("upstream-hyper disabled")`.

#![allow(dead_code)]

use std::sync::Arc;

#[cfg(not(feature = "upstream-hyper"))]
pub struct UpstreamClient;

#[cfg(not(feature = "upstream-hyper"))]
impl UpstreamClient {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
    pub async fn call(
        &self,
        _spec: UpstreamRequest,
        _profile: TimeoutProfile,
        _cancel: CancellationToken,
    ) -> Result<UpstreamResponse, UpstreamError> {
        Err(UpstreamError::Invalid(
            "upstream-hyper feature is disabled in this build".to_string(),
        ))
    }
}

#[derive(Debug, Clone, Default)]
pub struct CancellationToken;
impl CancellationToken {
    pub fn new() -> Self {
        Self
    }
    pub fn cancel(&self) {}
    pub fn is_cancelled(&self) -> bool {
        false
    }
    pub fn child(&self) -> Self {
        Self
    }
    pub fn cancelled(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(std::future::pending())
    }
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<bool> {
        let (_, rx) = tokio::sync::watch::channel(false);
        rx
    }
    pub fn from_watch(_rx: tokio::sync::watch::Receiver<bool>) -> Self {
        Self
    }
    pub fn from_watch_and_token(
        _rx: tokio::sync::watch::Receiver<bool>,
        _race_token: CancellationToken,
    ) -> Self {
        Self
    }
}

#[derive(Debug, Clone, Default)]
pub struct UpstreamRequest;
impl UpstreamRequest {
    pub fn get(_url: impl Into<String>) -> Self {
        Self
    }
    pub fn post_json(_url: impl Into<String>, _body: bytes::Bytes) -> Self {
        Self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutProfile {
    Chat,
    Quota,
    OAuth,
    ModelDiscovery,
    ImageGeneration,
    Custom(ResolvedTimeouts),
}
impl TimeoutProfile {
    pub fn resolve(&self) -> ResolvedTimeouts {
        ResolvedTimeouts::default()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResolvedTimeouts {
    pub dns_ms: u64,
    pub dial_ms: u64,
    pub tls_ms: u64,
    pub write_ms: u64,
    pub headers_ms: u64,
    pub body_chunk_ms: u64,
    pub total_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamPhase {
    Dns,
    Dial,
    Tls,
    Write,
    Headers,
    Body,
}
impl UpstreamPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            UpstreamPhase::Dns => "dns",
            UpstreamPhase::Dial => "dial",
            UpstreamPhase::Tls => "tls",
            UpstreamPhase::Write => "write",
            UpstreamPhase::Headers => "headers",
            UpstreamPhase::Body => "body",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ResolvedPhaseDeadlines;

#[derive(Debug)]
pub enum UpstreamError {
    Timeout(UpstreamPhase),
    Connection(String),
    Tls(String),
    Cancel,
    Http(String),
    Decode(String),
    Invalid(String),
}
impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpstreamError::Timeout(p) => write!(f, "timeout in {p:?}"),
            UpstreamError::Connection(m) => write!(f, "connection: {m}"),
            UpstreamError::Tls(m) => write!(f, "tls: {m}"),
            UpstreamError::Cancel => f.write_str("cancel"),
            UpstreamError::Http(m) => write!(f, "http: {m}"),
            UpstreamError::Decode(m) => write!(f, "decode: {m}"),
            UpstreamError::Invalid(m) => write!(f, "invalid: {m}"),
        }
    }
}
impl std::error::Error for UpstreamError {}

pub type UpstreamResult<T> = Result<T, UpstreamError>;

#[derive(Debug)]
pub struct UpstreamResponse;
impl UpstreamResponse {
    pub async fn collect(self) -> UpstreamResult<bytes::Bytes> {
        Ok(bytes::Bytes::new())
    }
}

#[derive(Debug)]
pub struct UpstreamBodyStream;

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct HostKey;
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

#[derive(Debug, Default, Clone)]
pub struct UpstreamConnectionPool;
impl UpstreamConnectionPool {
    pub fn new() -> Self {
        Self
    }
    pub fn reuses(&self) -> usize {
        0
    }
    pub fn total(&self) -> usize {
        0
    }
    pub fn host_count(&self) -> usize {
        0
    }
    pub fn reuses_for(&self, _key: &HostKey) -> usize {
        0
    }
    pub fn record_dial(&self, _key: HostKey) {}
    pub fn record_reuse(&self, _key: HostKey) {}
    pub fn evict_older_than(&self, _max_age: std::time::Duration) -> usize {
        0
    }
}
