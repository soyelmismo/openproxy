//! `tracing` / `tracing-subscriber` initialization.
//!
//! Respects `RUST_LOG` if set (highest priority), otherwise falls back to
//! the `level` field of the parsed [`LoggingConfig`]. Output format is
//! controlled by [`LogFormat`]: JSON for production, compact text for
//! development.

use openproxy_core::config::{LogFormat, LoggingConfig};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Initialize the global subscriber. Idempotent in the sense that calling
/// it twice is a no-op the second time, but in practice `main` is the
/// only caller.
///
/// Returns `Err` only if the configured filter or layer set is invalid;
/// the underlying `try_init` failure is propagated unchanged.
pub fn init(config: &LoggingConfig) -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&config.level));

    let registry = tracing_subscriber::registry().with(filter);

    match config.format {
        LogFormat::Json => {
            registry
                .with(fmt::layer().json().with_current_span(true).with_span_list(false))
                .try_init()?;
        }
        LogFormat::Text => {
            registry.with(fmt::layer().compact()).try_init()?;
        }
    }
    Ok(())
}
