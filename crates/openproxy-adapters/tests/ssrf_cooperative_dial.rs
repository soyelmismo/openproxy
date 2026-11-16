//! Integration tests for the cooperative transport's SSRF opt-in plumbing.
//!
//! These run WITHOUT `cfg(test)` for the crate itself, so the real
//! `filter_addrs_under_policy` logic executes (unit tests inside the crate
//! get `allow_all = cfg!(test)` and never exercise the filter). The contract:
//!
//! * loopback + flag    → dials
//! * loopback w/o flag  → SSRF block before any dial
//! * RFC1918 + flag     → dials (migration 000135)
//! * link-local + flag  → still blocked (IMDS, audit A6-01)
//!
//! Everything rides the default `CooperativeH1Transport` (plain HTTP, no
//! proxy), the same path discovery ticks and local chat dispatch use.

use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

/// Minimal HTTP/1.1 responder: accepts one request per call, replies 200.
fn spawn_http_server(listener: TcpListener) {
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 4096];
            // Drain the request head (single read is enough for these probes).
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
            let _ = stream.flush();
        }
    });
}

fn request(url: String, allow_loopback: bool) -> UpstreamRequest {
    UpstreamRequest {
        method: http::Method::GET,
        url,
        headers: http::HeaderMap::new(),
        body: None,
        is_streaming: false,
        proxy: None,
        proxy_status: None,
        allow_loopback,
    }
}

/// Bind a loopback listener and return its URL (127.0.0.1 always dials via
/// the cooperative transport).
fn loopback_listener() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("addr").port();
    spawn_http_server(listener);
    format!("http://127.0.0.1:{port}/")
}

fn is_ssrf_block(error: &str) -> bool {
    error.contains("private/reserved (SSRF block)")
}

/// Whether the real SSRF filter is compiled in for THIS test binary.
///
/// `cargo test` with several workspace crates selected (`-p db -p server ...`)
/// unifies features: any crate's dev-dependency `features = ["ssrf-bypass"]`
/// on openproxy-adapters switches the filter off for EVERY test binary of the
/// build, including this one (see openproxy-server/Cargo.toml for the pattern).
/// Under that build the block-path assertions below cannot hold — they are
/// covered by `cargo test -p openproxy-adapters alone (CI runs it per-crate).
async fn ssrf_filter_active() -> bool {
    let url = loopback_listener();
    let client = UpstreamClient::new();
    let probed = client
        .call(
            request(url, false),
            TimeoutProfile::ModelDiscovery,
            CancellationToken::new(),
        )
        .await;
    probed.is_err()
}

#[tokio::test]
async fn loopback_dials_with_opt_in() {
    let url = loopback_listener();
    let client = UpstreamClient::new();
    let res = client
        .call(
            request(url, true),
            TimeoutProfile::ModelDiscovery,
            CancellationToken::new(),
        )
        .await;
    assert!(
        res.is_ok(),
        "loopback + allow_loopback must dial, got {res:?}"
    );
}

#[tokio::test]
async fn loopback_blocked_without_opt_in() {
    if !ssrf_filter_active().await {
        return; // ssrf-bypass unified into this build; crate-solo run covers the block path
    }
    let url = loopback_listener();
    let client = UpstreamClient::new();
    let err = client
        .call(
            request(url, false),
            TimeoutProfile::ModelDiscovery,
            CancellationToken::new(),
        )
        .await
        .expect_err("loopback without opt-in must be SSRF-blocked");
    assert!(is_ssrf_block(&err.to_string()), "unexpected error: {err}");
}

#[tokio::test]
async fn rfc1918_admitted_with_opt_in_but_refused_without_listener() {
    // No listener on this RFC1918 address: the filter must ADMIT it (the dial
    // then fails with a connection error, not an SSRF block).
    let client = UpstreamClient::new();
    let err = client
        .call(
            request("http://10.240.0.21:1/".to_string(), true),
            TimeoutProfile::ModelDiscovery,
            CancellationToken::new(),
        )
        .await
        .expect_err("nothing listens there; dial must fail");
    let text = err.to_string();
    assert!(
        !is_ssrf_block(&text),
        "RFC1918 + opt-in must pass the filter, got: {text}"
    );
}

#[tokio::test]
async fn link_local_stays_blocked_even_with_opt_in() {
    if !ssrf_filter_active().await {
        return; // ssrf-bypass unified into this build; crate-solo run covers the block path
    }
    // IMDS (169.254/16) must never dial, even with the per-provider opt-in.
    let client = UpstreamClient::new();
    let err = client
        .call(
            request("http://169.254.169.254/latest/meta-data/".to_string(), true),
            TimeoutProfile::ModelDiscovery,
            CancellationToken::new(),
        )
        .await
        .expect_err("link-local must be blocked even with opt-in");
    assert!(is_ssrf_block(&err.to_string()), "unexpected error: {err}");
}
