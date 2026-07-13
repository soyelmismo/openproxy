//! `tracing` / `tracing-subscriber` initialization.
//!
//! Respects `RUST_LOG` if set (highest priority), otherwise falls back to
//! the `level` field of the parsed [`LoggingConfig`]. Output format is
//! controlled by [`LogFormat`]: JSON for production, compact text for
//! development.
//!
//! In addition to the stdout `fmt` layer, we install a
//! [`DebugLogLayer`](crate::debug_log::DebugLogLayer) that captures every
//! event into an in-memory ring buffer exposed to the dashboard via
//! `GET /admin/debug/logs`. See `debug_log.rs` for the full rationale.
//!
//! ## B1 (Bug 3): per-layer filtering
//!
//! The `fmt` layer (stdout) honors the operator's `RUST_LOG` (or the
//! `LoggingConfig.level` default of `"info"`). The `DebugLogLayer` is
//! given its OWN per-layer filter (`LevelFilter::WARN`) so it ALWAYS
//! captures WARN+ERROR events into the ring buffer — even when the
//! operator sets `RUST_LOG=error` (silencing WARN from stdout) or
//! `RUST_LOG=off` (silencing everything). Without this, the dashboard's
//! Debug Logs view would silently miss discovery-tick failures and other
//! WARN-level events that the operator needed to see in order to
//! troubleshoot upstream 404s like the Cloudflare account-id bug.

use openproxy_core::config::{LogFormat, LoggingConfig};
use tracing_subscriber::{EnvFilter, filter::LevelFilter, fmt, prelude::*};

/// Initialize the global subscriber. Idempotent in the sense that calling
/// it twice is a no-op the second time, but in practice `main` is the
/// only caller.
///
/// Returns `Err` only if the configured filter or layer set is invalid;
/// the underlying `try_init` failure is propagated unchanged.
pub fn init(config: &LoggingConfig) -> anyhow::Result<()> {
    // Initialize the in-memory debug-log ring buffer FIRST so the
    // DebugLogLayer can push into it as soon as the subscriber is
    // installed. `debug_log::init` is idempotent.
    crate::debug_log::init();

    // The fmt layer's filter: operator-controlled via RUST_LOG, with
    // a fallback to `LoggingConfig.level` (default "info"). This is
    // the only filter that controls stdout output.
    let fmt_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.level));

    // The DebugLogLayer's filter: ALWAYS WARN+. This is independent
    // of `fmt_filter` (per-layer filters apply to their OWN layer
    // only) — so even when the operator sets `RUST_LOG=error` (which
    // would silence WARN from stdout), the DebugLogLayer still
    // captures WARN events into the ring buffer for the dashboard's
    // Debug Logs view. `LevelFilter` accepts all events at or above
    // the given level (WARN, ERROR).
    //
    // NOTE: the `with_filter` call is constructed INSIDE each match
    // arm rather than once before the match. The reason is subtle:
    // `Layer::with_filter` returns `Filtered<Self, F, S>` where `S`
    // is the layer's subscriber-type parameter, and that parameter
    // must be inferable at the point of construction. If we build
    // the filtered layer once before the `match`, the compiler
    // commits to a single `S` inferred from the first usage site
    // (the JSON branch), and the second usage site (the Text branch)
    // fails to unify because its `fmt::Layer` uses
    // `DefaultFields`+`Format<Compact>` rather than
    // `JsonFields`+`Format<Json>`. Constructing the filtered
    // `DebugLogLayer` separately in each arm lets the compiler infer
    // the right `S` for each arm.
    let debug_filter = LevelFilter::WARN;

    match config.format {
        LogFormat::Json => {
            tracing_subscriber::registry()
                .with(
                    fmt::layer()
                        .json()
                        .with_current_span(true)
                        .with_span_list(false)
                        .with_filter(fmt_filter),
                )
                .with(crate::debug_log::DebugLogLayer.with_filter(debug_filter))
                .try_init()?;
        }
        LogFormat::Text => {
            tracing_subscriber::registry()
                .with(fmt::layer().compact().with_filter(fmt_filter))
                .with(crate::debug_log::DebugLogLayer.with_filter(debug_filter))
                .try_init()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_core::config::{LogFormat, LoggingConfig};

    #[tokio::test]
    async fn test_telemetry_init_text_format() {
        let config = LoggingConfig {
            format: LogFormat::Text,
            level: "info".to_string(),
        };
        // The first call might succeed or fail if the subscriber is already set by another test.
        let _ = init(&config);

        // The second call is guaranteed to fail because the global subscriber is definitely set.
        let result = init(&config);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_telemetry_init_json_format() {
        let config = LoggingConfig {
            format: LogFormat::Json,
            level: "info".to_string(),
        };
        let _ = init(&config);

        let result = init(&config);
        assert!(result.is_err());
    }

    #[test]
    fn test_telemetry_init_invalid_level_does_not_panic() {
        // Since we can't reliably test the full `init()` function with an invalid level
        // (because `tracing_subscriber::registry().try_init()` returns an error if another
        // test already initialized the subscriber, which is common in parallel tests),
        // we instead verify that parsing a garbage level via EnvFilter does not panic.
        // The `EnvFilter::new()` and `try_from_default_env().unwrap_or_else` code inside
        // `init()` will gracefully treat invalid syntax as a valid target (e.g., target="invalid_level_!!!")
        // rather than panicking.
        let filter = EnvFilter::new("invalid_level_!!!");
        assert_eq!(
            filter.to_string(),
            "invalid_level_!!!=trace" // default level for a custom target
        );
    }
}
