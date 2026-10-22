//! `UpstreamClient`: a hyper-based HTTP client with per-phase timeouts and a
//! per-host connection pool.
//!
//! Gated by the `upstream-hyper` feature (default-on in `openproxy-core`).
//! With the feature off, `stubs` keeps the public surface resolving and `call`
//! returns `UpstreamError::Invalid("upstream-hyper disabled")`.
//!
//! Deviations from the spec:
//!
//! - **Connection pool primitive.** hyper 1.10's `SendRequest` is not `Clone`
//!   and owns its half of the connection, so a shared map would need `&mut`
//!   per send. The primitive is `hyper_util::client::legacy::Client`; the
//!   user-facing `UpstreamConnectionPool` surface (`reuses()`, `total()`,
//!   `Default`) is unchanged. Rationale in `conn_pool.rs`.
//! - **Granular TLS timeout.** The spec path collapses DNS, dial and TLS into
//!   one `Service::call` future, so a production stall attributes to
//!   `UpstreamPhase::Headers`; the test connector in `tests.rs` reports the
//!   stalling phase directly.
//! - **Body limit.** `NON_STREAMING_BODY_LIMIT_BYTES` (32 MiB) caps memory
//!   when buffering a whole payload. `STREAMING_BODY_LIMIT_BYTES` is
//!   unbounded, since chunks go straight to the client under idle-chunk and
//!   total timeouts.
//! - **Transparent content-encoding.** `gzip` and `deflate` are negotiated and
//!   inverted here, once, for every provider and request shape; see
//!   `compression.rs` for the codec scope and for why `br`/`zstd` are not
//!   advertised. The response headers the caller sees describe plaintext, not
//!   the wire.

#[cfg(feature = "upstream-hyper")]
mod cancel;
#[cfg(feature = "upstream-hyper")]
mod client;
#[cfg(feature = "upstream-hyper")]
mod compression;
#[cfg(feature = "upstream-hyper")]
mod conn_pool;
#[cfg(feature = "upstream-hyper")]
mod connector;
#[cfg(feature = "upstream-hyper")]
pub(crate) mod connector_types;
#[cfg(feature = "upstream-hyper")]
pub(crate) mod dns;
#[cfg(feature = "upstream-hyper")]
mod error;
#[cfg(feature = "upstream-hyper")]
mod phases;
#[cfg(feature = "upstream-hyper")]
mod profile;
#[cfg(feature = "upstream-hyper")]
pub(crate) mod proxy_tunnel;
#[cfg(feature = "upstream-hyper")]
mod request_body;
#[cfg(feature = "upstream-hyper")]
mod response;
#[cfg(feature = "upstream-hyper")]
mod upload;

#[cfg(feature = "upstream-hyper")]
mod tests;

#[cfg(feature = "upstream-hyper")]
#[cfg(test)]
mod compression_tests;

#[cfg(feature = "upstream-hyper")]
#[cfg(test)]
mod compression_http_tests;

#[cfg(all(feature = "upstream-hyper", test))]
mod upload_tests;

#[cfg(feature = "upstream-hyper")]
#[cfg(test)]
pub mod tests_helper;

#[cfg(feature = "upstream-hyper")]
pub use cancel::CancellationToken;
#[cfg(feature = "upstream-hyper")]
pub use client::{
    UpstreamClient, UpstreamRequest, bearer_auth_header, bearer_header_value, format_bearer,
};
#[cfg(feature = "upstream-hyper")]
pub use conn_pool::{HostKey, Scheme, UpstreamConnectionPool};
#[cfg(feature = "upstream-hyper")]
pub use dns::resolve_public_host;

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
pub use response::{
    NON_STREAMING_BODY_LIMIT_BYTES, STREAMING_BODY_LIMIT_BYTES, UpstreamBodyStream,
    UpstreamResponse,
};

// Stubs for builds with the feature disabled, so
// `crate::upstream::UpstreamClient` and friends always resolve.

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

/// Helper for integration tests to load upstream source files directly via HTTP from remote
/// repositories (e.g. GitHub Raw, npm) with an optional local file fallback.
#[cfg(any(test, feature = "test-utils"))]
pub async fn load_upstream_source(http_url: &str, local_rel_path: &str) -> Option<String> {
    let client = UpstreamClient::new();
    let cancel = CancellationToken::new();
    let req = UpstreamRequest::get(http_url);
    if let Ok(resp) = client.call(req, TimeoutProfile::OAuth, cancel).await
        && resp.status.is_success()
        && let Ok(body) = resp.collect().await
    {
        return Some(String::from_utf8_lossy(&body).into_owned());
    }

    let base_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let path = std::path::Path::new(&base_dir).join(local_rel_path);
    if path.exists() {
        return std::fs::read_to_string(&path).ok();
    }

    None
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
