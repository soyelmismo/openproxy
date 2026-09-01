//! `UpstreamClient` — a hyper-based HTTP client with per-phase timeouts
//! and a per-host connection pool.
//!
//! This module is gated by the `upstream-hyper` feature (default-on in
//! `openproxy-core`'s `Cargo.toml`). Disabling the feature at build time
//! keeps this module out of the compilation; re-exports below are
//! stubs that satisfy the public API surface but return
//! `UpstreamError::Invalid("upstream-hyper disabled")` from `call`.
//!
//! ## Deviations from the spec
//!
//! - **Connection pool primitive.** The spec describes a
//!   `Mutex<HashMap<HostKey, hyper::client::conn::http1::SendRequest>>`.
//!   `SendRequest` in hyper 1.10 is **not** `Clone` and owns its half
//!   of the connection, so holding it in a shared map would force
//!   `&mut` access for every send. The actual primitive used is
//!   `hyper_util::client::legacy::Client` (which is `Clone` and shares
//!   an internal per-host pool). The user-facing surface is unchanged:
//!   `UpstreamConnectionPool` exposes `reuses()` and `total()` counters
//!   and a `Default` impl, matching the spec's intent. See
//!   `conn_pool.rs` for the full rationale.
//!
//! - **Granular TLS timeout.** hyper 1.10 has no per-phase timeout on
//!   the `HttpConnector` / `HttpsConnector` path: DNS, dial, and TLS
//!   are a single `Service::call` future. To attribute a stalled
//!   `UpstreamError::Timeout(phase)` to the right step, the
//!   unit-test connector in `tests.rs` reports the stalling phase
//!   directly. The production `DefaultConnector` reports a
//!   single `Connection` error and the client attributes it to
//!   `UpstreamPhase::Headers` (the closest phase boundary that
//!   includes connect+TLS). Splitting connect from TLS in production
//!   requires a custom DNS resolver and is a follow-up gate.
//!
//! - **Body limit.** A 32 MiB hard cap is applied to every body via
//!   `http_body_util::Limited`. This is a Gate-0 safety belt; the
//!   real limit will be a config knob in a follow-up gate.

#[cfg(feature = "upstream-hyper")]
mod cancel;
#[cfg(feature = "upstream-hyper")]
mod client;
#[cfg(feature = "upstream-hyper")]
mod conn_pool;
#[cfg(feature = "upstream-hyper")]
mod connector;
#[cfg(feature = "upstream-hyper")]
mod error;
#[cfg(feature = "upstream-hyper")]
mod phases;
#[cfg(feature = "upstream-hyper")]
mod profile;
#[cfg(feature = "upstream-hyper")]
mod response;

#[cfg(feature = "upstream-hyper")]
mod tests;

#[cfg(feature = "upstream-hyper")]
#[cfg(test)]
pub use tests::tests_helper;

#[cfg(feature = "upstream-hyper")]
pub use cancel::CancellationToken;
#[cfg(feature = "upstream-hyper")]
pub use client::{UpstreamClient, UpstreamRequest};
#[cfg(feature = "upstream-hyper")]
pub use conn_pool::{HostKey, Scheme, UpstreamConnectionPool};
#[cfg(feature = "upstream-hyper")]
pub use connector::{
    PhasedConnector, PhasedConnectorError, PhasedTimeouts, is_private_or_reserved, phased_phase,
};
#[cfg(feature = "upstream-hyper")]
pub use error::{UpstreamError, UpstreamResult};
#[cfg(feature = "upstream-hyper")]
pub use phases::{ResolvedPhaseDeadlines, UpstreamPhase};
#[cfg(feature = "upstream-hyper")]
pub use profile::{ResolvedTimeouts, TimeoutProfile};
#[cfg(feature = "upstream-hyper")]
pub use response::{UpstreamBodyStream, UpstreamResponse};

// -- Stubs for builds with the feature disabled -----------------------------
//
// When `upstream-hyper` is off, the module compiles but the types are
// not constructible. We provide marker types and re-exports so that
// `crate::upstream::UpstreamClient` etc. always resolve (call sites
// that aren't yet migrated don't notice the difference; new code can
// branch on the feature to use real types).

#[cfg(not(feature = "upstream-hyper"))]
mod stubs;
#[cfg(not(feature = "upstream-hyper"))]
pub use stubs::*;

/// Macro for generating $O(1)$ compile-time `const fn` (or `fn`) jump tables for lookup and mapping.
///
/// Supports generating:
/// - Functions returning `Option<V>` with automatic `None` fallback when no wildcard is provided.
/// - Functions returning `V` with explicit pattern matching (including wildcard fallback `_ => default`).
#[macro_export]
macro_rules! define_jump_map {
    (
        $(#[$meta:meta])*
        $vis:vis const fn $fn_name:ident($key_param:ident: $key_ty:ty) -> Option<$val_ty:ty> {
            $( $key:pat $(if $guard:expr)? => $val:expr ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        $vis const fn $fn_name($key_param: $key_ty) -> Option<$val_ty> {
            match $key_param {
                $( $key $(if $guard)? => Some($val), )*
                #[allow(unreachable_patterns)]
                _ => None,
            }
        }
    };
    (
        $(#[$meta:meta])*
        $vis:vis const fn $fn_name:ident($key_param:ident: $key_ty:ty) -> $val_ty:ty {
            $( $key:pat $(if $guard:expr)? => $val:expr ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        $vis const fn $fn_name($key_param: $key_ty) -> $val_ty {
            match $key_param {
                $( $key $(if $guard)? => $val, )*
            }
        }
    };
    (
        $(#[$meta:meta])*
        $vis:vis fn $fn_name:ident($key_param:ident: $key_ty:ty) -> Option<$val_ty:ty> {
            $( $key:pat $(if $guard:expr)? => $val:expr ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        $vis fn $fn_name($key_param: $key_ty) -> Option<$val_ty> {
            match $key_param {
                $( $key $(if $guard)? => Some($val), )*
                #[allow(unreachable_patterns)]
                _ => None,
            }
        }
    };
    (
        $(#[$meta:meta])*
        $vis:vis fn $fn_name:ident($key_param:ident: $key_ty:ty) -> $val_ty:ty {
            $( $key:pat $(if $guard:expr)? => $val:expr ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        $vis fn $fn_name($key_param: $key_ty) -> $val_ty {
            match $key_param {
                $( $key $(if $guard)? => $val, )*
            }
        }
    };
}

#[cfg(test)]
mod jump_map_tests {
    define_jump_map! {
        /// Lookup provider endpoint path by name
        pub fn provider_path(name: &str) -> Option<&'static str> {
            "openai" => "/v1/chat/completions",
            "anthropic" => "/v1/messages",
            "gemini" => "/v1beta/models",
        }
    }

    define_jump_map! {
        /// Categorize HTTP status code with default fallback
        pub const fn status_group(code: u16) -> &'static str {
            200..=299 => "2xx",
            400..=499 => "4xx",
            500..=599 => "5xx",
            _ => "other",
        }
    }

    define_jump_map! {
        /// Check for known success codes in const fn
        pub const fn is_known_success(code: u16) -> Option<&'static str> {
            200 => "OK",
            201 => "Created",
            204 => "No Content",
        }
    }

    #[test]
    fn test_define_jump_map_lookup() {
        assert_eq!(provider_path("openai"), Some("/v1/chat/completions"));
        assert_eq!(provider_path("anthropic"), Some("/v1/messages"));
        assert_eq!(provider_path("gemini"), Some("/v1beta/models"));
        assert_eq!(provider_path("unknown"), None);

        const STATUS_OK: &str = status_group(200);
        assert_eq!(STATUS_OK, "2xx");
        assert_eq!(status_group(404), "4xx");
        assert_eq!(status_group(503), "5xx");
        assert_eq!(status_group(101), "other");

        const OK_MSG: Option<&str> = is_known_success(200);
        assert_eq!(OK_MSG, Some("OK"));
        assert_eq!(is_known_success(201), Some("Created"));
        assert_eq!(is_known_success(500), None);
    }
}
