//! openproxy-server: HTTP server.
//!
//! Wires [`axum`] routes, middleware, and shared state around the
//! `openproxy-core` library. The binary entry point lives in `main.rs`;
//! everything else is reachable through the [`openproxy_server`] crate.

pub mod state;
pub mod error;
pub mod telemetry;
pub mod middleware;
pub mod router;
pub mod handlers;
pub mod disconnect;
