#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Comprehensive Opaque-Box E2E Test Suite for openproxy.
//!
//! Covers 4 tiers of testing per Project Pattern:
//! - Tier 1: Feature Coverage (Proxy, Admin, Streaming, Compression)
//! - Tier 2: Boundary & Corner Cases (Payload limits, Auth, Disconnects, Timeouts/Retries)
//! - Tier 3: Cross-Feature Combinations (Combos, Failover, Cooldown, Streaming, Auth, Usage)
//! - Tier 4: Real-World Application Scenarios (Multi-turn conversations, Discovery sync, Multi-client analytics, Races, Admin lifecycle)

pub mod harness;
pub mod tier1_features;
pub mod tier2_boundaries;
pub mod tier3_combinations;
pub mod tier4_scenarios;
