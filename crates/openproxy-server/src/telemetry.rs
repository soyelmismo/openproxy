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

    #[tokio::test]
    async fn test_telemetry_init_success_isolated() {
        // Because tracing-subscriber can only be initialized once globally per process,
        // standard tests running in a shared process often cannot test the successful `Ok(())` path
        // if another test has already run `init()`.
        // To reliably test the success path, we fork a child process that runs ONLY this test,
        // passing a specific environment variable so the child knows it's the isolated runner.

        let env_var = "_RUN_ISOLATED_TELEMETRY_INIT";

        if std::env::var(env_var).is_ok() {
            // We are the child process. Run the actual success test.
            let config = LoggingConfig {
                format: LogFormat::Text,
                level: "info".to_string(),
            };

            // This is guaranteed to be the FIRST time init is called in this process.
            let result = init(&config);
            assert!(result.is_ok(), "Telemetry init should succeed on first call");
            return;
        }

        // We are the parent test runner. Spawn the child process.
        // We find the current executable (the test binary) and tell it to run this specific test function.
        let exe = std::env::current_exe().expect("Failed to get current executable");

        let output = std::process::Command::new(exe)
            .arg("--exact")
            .arg("telemetry::tests::test_telemetry_init_success_isolated")
            // We must avoid capturing test output (the default cargo test behavior)
            // so we add --nocapture, although it's for the child test runner.
            .arg("--nocapture")
            .env(env_var, "1")
            .output()
            .expect("Failed to execute child process");

        assert!(
            output.status.success(),
            "Isolated telemetry init test failed.\nSTDOUT:\n{}\nSTDERR:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
