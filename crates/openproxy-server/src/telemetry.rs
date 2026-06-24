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

use openproxy_core::config::{LogFormat, LoggingConfig};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

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

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.level));

    let registry = tracing_subscriber::registry().with(filter);

    // The DebugLogLayer captures every event into the in-memory ring
    // buffer. It's installed alongside the fmt layer so stdout output
    // is unaffected.
    let debug_layer = crate::debug_log::DebugLogLayer;

    match config.format {
        LogFormat::Json => {
            registry
                .with(
                    fmt::layer()
                        .json()
                        .with_current_span(true)
                        .with_span_list(false),
                )
                .with(debug_layer)
                .try_init()?;
        }
        LogFormat::Text => {
            registry
                .with(fmt::layer().compact())
                .with(debug_layer)
                .try_init()?;
        }
    }
    Ok(())
}
