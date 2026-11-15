//! PoC / regression: decompression bombs in the one-shot codecs (`br`,
//! `zstd`) must be rejected WITHOUT buffering the full expansion in memory.
//!
//! Vulnerability being pinned (security audit, H-1): `finish_one_shot`
//! decompressed the entire accumulated wire buffer into an unbounded `Vec`
//! and only compared the result against `MAX_DECODED_BODY_BYTES` afterwards.
//! A few hundred bytes of brotli (ratio >500000:1 over repetitive data) could
//! therefore claim gigabytes of resident memory before the post-hoc check
//! fired. The tests here measure PEAK LIVE ALLOCATIONS on the decoding thread
//! while the bomb is decoded: the fix must reject the bomb with peak memory
//! close to the cap, not to the bomb's expanded size.
//!
//! A counting `GlobalAlloc` tracks net live bytes per thread while a
//! `TrackPeak` guard is held, so parallel tests and background threads do not
//! pollute the measurement.

#![cfg(all(feature = "upstream-hyper", test))]

use super::compression::Codec;
use super::compression_tests::{drive, frames_of};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

/// Brotli stream produced by the reference encoder (`brotli -q 11`) over
/// ~123 MiB of repetitive data: 230 bytes on the wire, ratio 559238:1.
/// Real encoder output, so the decoder exercises production bytes.
const BROTLI_BOMB: &[u8] = include_bytes!("fixtures/brotli_bomb.bin");

/// Expanded size of [`BROTLI_BOMB`], measured when the fixture was generated.
const BROTLI_BOMB_EXPANDED_BYTES: u64 = 123 * 1024 * 1024;

struct TrackState {
    live: Cell<u64>,
    peak: Cell<u64>,
    on: Cell<bool>,
}

thread_local! {
    static TRACK: TrackState = const { TrackState {
        live: Cell::new(0),
        peak: Cell::new(0),
        on: Cell::new(false),
    } };
}

struct CountingAlloc;

#[inline]
fn track_alloc(size: u64) {
    // `try_with`: TLS can be destroyed during thread teardown while the
    // allocator is still in use; in that case tracking is simply off.
    let _ = TRACK.try_with(|s| {
        if s.on.get() {
            let now = s.live.get().wrapping_add(size);
            s.live.set(now);
            if now > s.peak.get() {
                s.peak.set(now);
            }
        }
    });
}

#[inline]
fn track_dealloc(size: u64) {
    let _ = TRACK.try_with(|s| {
        if s.on.get() {
            s.live.set(s.live.get().wrapping_sub(size));
        }
    });
}

#[inline]
fn track_realloc(old_size: usize, new_size: usize) {
    let _ = TRACK.try_with(|s| {
        if s.on.get() {
            let delta = new_size as i64 - old_size as i64;
            let now = if delta >= 0 {
                s.live.get().wrapping_add(delta as u64)
            } else {
                s.live.get().wrapping_sub((-delta) as u64)
            };
            s.live.set(now);
            if now > s.peak.get() {
                s.peak.set(now);
            }
        }
    });
}

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarding to the system allocator with a caller-valid layout.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            track_alloc(layout.size() as u64);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        track_dealloc(layout.size() as u64);
        // SAFETY: forwarding to the system allocator with a caller-valid pointer.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarding to the system allocator with caller-valid arguments.
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            track_realloc(layout.size(), new_size);
        }
        new_ptr
    }
}

#[global_allocator]
static GLOBAL_ALLOC: CountingAlloc = CountingAlloc;

/// Track peak live allocations on THIS thread until dropped.
struct TrackPeak;

impl TrackPeak {
    fn arm() -> Self {
        TRACK.with(|s| {
            s.live.set(0);
            s.peak.set(0);
            s.on.set(true);
        });
        TrackPeak
    }

    fn peak_bytes(&self) -> u64 {
        TRACK.with(|s| s.peak.get())
    }
}

impl Drop for TrackPeak {
    fn drop(&mut self) {
        TRACK.with(|s| s.on.set(false));
    }
}

#[test]
fn brotli_bomb_rejected_without_full_expansion() {
    let guard = TrackPeak::arm();
    let result = drive(Some(Codec::Brotli), &frames_of(BROTLI_BOMB, 64));
    let peak = guard.peak_bytes();
    drop(guard);

    let err = result.expect_err("bomb expanding past the decoded cap must fail");
    assert!(
        err.contains("expands") || err.contains("decoded"),
        "unexpected error: {err}"
    );
    // The bomb expands to ~123 MiB; the vulnerable code buffered ALL of it
    // (peak 138.5 MB, measured) before rejecting. The fixed code stops at the
    // 32 MiB decoded cap: the output Vec may double up to 64 MiB of capacity
    // while crossing the cap, so the bound is ~2x cap REGARDLESS of the
    // bomb's expansion factor — constant, not proportional to the attack.
    assert!(
        peak < 72 * 1024 * 1024,
        "peak live allocations reached {peak} bytes while decoding a 230-byte \
         brotli bomb: the expansion was buffered in full instead of being \
         capped incrementally"
    );
    assert!(
        peak < BROTLI_BOMB_EXPANDED_BYTES,
        "peak {peak} bytes means the bomb was fully materialised"
    );
}

#[test]
fn zstd_bomb_rejected_without_full_expansion() {
    // 128 MiB of repetitive data compressed with the real zstd encoder.
    let payload = vec![b'z'; 128 * 1024 * 1024];
    let bomb = zstd::encode_all(&payload[..], 19).expect("zstd encode");
    drop(payload);

    let guard = TrackPeak::arm();
    let result = drive(Some(Codec::Zstd), &frames_of(&bomb, 64));
    let peak = guard.peak_bytes();
    drop(guard);

    let err = result.expect_err("bomb expanding past the decoded cap must fail");
    assert!(
        err.contains("expands") || err.contains("decoded"),
        "unexpected error: {err}"
    );
    assert!(
        peak < 72 * 1024 * 1024,
        "peak live allocations reached {peak} bytes: the zstd bomb's expansion \
         was buffered in full instead of being capped incrementally"
    );
}

#[test]
fn one_shot_wire_accumulation_is_capped_for_streaming_bodies() {
    // A streaming response answered `content-encoding: zstd` even though zstd
    // was never advertised (the transport limit for streams is u64::MAX). The
    // one-shot accumulator must still refuse to buffer without bound: ~33 MiB
    // of incompressible (xorshift) data keeps ~33 MiB on the wire, past the
    // 32 MiB accumulation cap. The decoded cap cannot be what rejects this
    // body — the expansion ratio is ~1:1 — so this pins the wire cap itself.
    let mut payload = Vec::with_capacity(33 * 1024 * 1024);
    let mut x: u32 = 0x9E37_79B9;
    while payload.len() < 33 * 1024 * 1024 {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        payload.extend_from_slice(&x.to_le_bytes());
    }
    let wire = zstd::encode_all(&payload[..], 19).expect("zstd encode");
    assert!(
        wire.len() as u64 > 32 * 1024 * 1024,
        "fixture must exceed the wire cap ({} bytes)",
        wire.len()
    );
    let result = drive(Some(Codec::Zstd), &frames_of(&wire, 256 * 1024));
    let err = result.expect_err("wire accumulation past the cap must fail");
    assert!(
        err.contains("buffered bytes"),
        "must fail at the wire cap, not elsewhere: {err}"
    );
}

/// End-to-end HTTP-level reproduction: a non-streaming request (the discovery
/// and OAuth profile — `Accept-Encoding: gzip, deflate, br, zstd`) served a
/// 230-byte brotli bomb. The transport must reject it with bounded memory.
#[tokio::test]
async fn http_brotli_bomb_rejected_with_bounded_memory() {
    use super::compression_http_tests::{body_response, spawn_responder};
    use super::{CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest};

    let addr = spawn_responder(move |_| {
        body_response(
            "200 OK",
            &[
                ("content-type", "application/json".to_string()),
                ("content-encoding", "br".to_string()),
                ("content-length", BROTLI_BOMB.len().to_string()),
            ],
            BROTLI_BOMB,
        )
    })
    .await;

    let mut spec = UpstreamRequest::get(format!("http://{addr}/models"));
    spec.is_streaming = false;
    let response = UpstreamClient::new()
        .call(
            spec,
            TimeoutProfile::ModelDiscovery,
            CancellationToken::new(),
        )
        .await
        .expect("call ok");
    assert_eq!(response.status.as_u16(), 200);

    let guard = TrackPeak::arm();
    let collected = response.collect().await;
    let peak = guard.peak_bytes();
    drop(guard);

    let err = collected.expect_err("bomb must be rejected");
    assert!(
        err.to_string().contains("expands"),
        "unexpected error: {err}"
    );
    assert!(
        peak < 72 * 1024 * 1024,
        "peak live allocations reached {peak} bytes over HTTP: the brotli \
         bomb's expansion was buffered in full"
    );
}
