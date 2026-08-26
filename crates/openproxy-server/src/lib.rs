//! openproxy-server: HTTP server.
//!
//! Wires [`axum`] routes, middleware, and shared state around the
//! `openproxy-core` library. The binary entry point lives in `main.rs`;
//! everything else is reachable through the [`openproxy_server`] crate.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

/// Dashboard SPA embedded in the server binary via `rust-embed`.
/// Serves `index.html`, `callback.html`, and the `dist/` /
/// `styles/` / `fonts/` asset tree at `/admin/*`. The admin REST
/// API lives at `/admin/api/*` (see `router.rs`) and the live-logs
/// WebSocket lives at `/admin/ws` — neither is served from this
/// module.
pub mod admin_ui;
pub mod background;
/// In-memory ring buffer of recent `tracing` events, exposed to the
/// dashboard via `GET /admin/debug/logs`. See `debug_log.rs` for the
/// full design rationale.
pub mod debug_log;
pub mod disconnect;
pub mod error;
pub mod extractors;
pub mod handlers;
pub mod middleware;
pub mod repositories;
pub mod router;
pub mod services;
pub mod state;
pub mod telemetry;

pub use background::{BackgroundService, BackgroundSupervisor};
