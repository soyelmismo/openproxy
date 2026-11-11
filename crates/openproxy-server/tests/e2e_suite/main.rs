#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Comprehensive Opaque-Box E2E Test Suite for openproxy.
//!
//! Covers 4 tiers of testing per Project Pattern:
//! - Tier 1: Feature Coverage (Proxy, Admin, Streaming, Compression)
//! - Tier 2: Boundary & Corner Cases (Payload limits, Auth, Disconnects, Timeouts/Retries)
//! - Tier 3: Cross-Feature Combinations (Combos, Failover, Cooldown, Streaming, Auth, Usage)
//! - Tier 4: Real-World Application Scenarios (Multi-turn conversations, Discovery sync, Multi-client analytics, Races, Admin lifecycle)

pub mod harness;
pub mod systemone_tests;
pub mod tier1_features;
pub mod tier2_boundaries;
pub mod tier3_combinations;
pub mod tier4_scenarios;

/// Runs an async test body on a dedicated thread with a large stack.
///
/// The decision pipeline builds a deep future (adapter + bridge + per-target
/// decision evaluation awaits nested inside each other), and debug builds inflate
/// every future in that chain. It overflows the 2 MiB stack libtest gives each
/// test thread. This is depth, not infinite recursion — the same body passes with
/// `RUST_MIN_STACK=33554432` — so the fix is a bigger stack for the tests that
/// need it, not a change to the pipeline.
pub fn run_with_big_stack<F, Fut>(body: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()>,
{
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            // current_thread + enable_all mirrors `#[tokio::test]`'s default, so
            // no test changes timing or spawning semantics.
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build tokio runtime")
                .block_on(body());
        })
        .expect("spawn big-stack test thread")
        .join()
        .expect("test thread panicked");
}
