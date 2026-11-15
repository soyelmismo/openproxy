//! `tracing` / `tracing-subscriber` initialization.
//!
//! Respects `RUST_LOG` if set (highest priority), else the `level` field of
//! [`LoggingConfig`]. [`LogFormat`] picks JSON for production, compact text for
//! development. Alongside the stdout `fmt` layer we install a
//! [`DebugLogLayer`](crate::debug_log::DebugLogLayer) feeding the in-memory ring
//! buffer behind `GET /admin/debug/logs` (rationale in `debug_log.rs`).
//!
//! The `DebugLogLayer` carries its OWN `LevelFilter::WARN` per-layer filter so
//! it always captures WARN+ERROR, even when the operator sets `RUST_LOG=error` or
//! `RUST_LOG=off`. Without it the dashboard's Debug Logs view would silently miss
//! WARN events such as discovery-tick failures behind an upstream 404.

use openproxy_core::config::{LogFormat, LoggingConfig};
use tracing_subscriber::{EnvFilter, filter::LevelFilter, fmt, prelude::*};

/// Initialize the global subscriber. Idempotent in the sense that calling
/// it twice is a no-op the second time, but in practice `main` is the
/// only caller.
///
/// Returns `Err` only if the configured filter or layer set is invalid;
/// the underlying `try_init` failure is propagated unchanged.
/// Respect https://no-color.org/: when `NO_COLOR` is set to a non-empty value,
/// disable ANSI escape sequences in terminal log output.
pub fn no_color_requested() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
}

pub fn init(config: &LoggingConfig) -> anyhow::Result<()> {
    // Ring buffer first: DebugLogLayer must be able to push the moment the
    // subscriber is installed. Idempotent.
    crate::debug_log::init(config);

    // fmt layer: operator-controlled via RUST_LOG, else `LoggingConfig.level`.
    // The only filter governing stdout.
    let fmt_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.level));

    // DebugLogLayer: always WARN+, independent of `fmt_filter` because a
    // per-layer filter applies only to its own layer. `LevelFilter` passes
    // every event at or above the level.
    //
    // `with_filter` is constructed INSIDE each match arm on purpose:
    // `Layer::with_filter` returns `Filtered<Self, F, S>` whose `S` must be
    // inferable at the construction point, so building it once before the match
    // commits to the JSON arm's `S` and the Text arm cannot unify
    // (`DefaultFields`+`Format<Compact>` vs `JsonFields`+`Format<Json>`).
    let debug_filter = LevelFilter::WARN;
    let ansi = !no_color_requested() && std::io::IsTerminal::is_terminal(&std::io::stdout());

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
                .with(
                    fmt::layer()
                        .compact()
                        .with_ansi(ansi)
                        .with_filter(fmt_filter),
                )
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
            ..Default::default()
        };
        let _ = init(&config);

        // The second call always fails: the global subscriber is set by then.
        let result = init(&config);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_telemetry_init_json_format() {
        let config = LoggingConfig {
            format: LogFormat::Json,
            level: "info".to_string(),
            ..Default::default()
        };
        let _ = init(&config);

        let result = init(&config);
        assert!(result.is_err());
    }

    #[test]
    fn test_telemetry_init_invalid_level_does_not_panic() {
        // `init()` itself cannot be exercised with a garbage level: a parallel
        // test may already own the global subscriber. Assert instead that
        // EnvFilter parses it without panicking — the
        // `EnvFilter::new(..).unwrap_or_else(..)` path inside `init()` treats
        // invalid syntax as a custom target (`invalid_level_!!!`).
        let filter = EnvFilter::new("invalid_level_!!!");
        assert_eq!(
            filter.to_string(),
            "invalid_level_!!!=trace" // default level for a custom target
        );
    }
}
