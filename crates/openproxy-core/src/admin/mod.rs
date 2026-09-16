//! Admin service layer. HTTP endpoints in openproxy-server call into this.
//!
//! Each function takes a `&rusqlite::Connection` and is intentionally free of
//! HTTP / axum concerns. The caller (the server crate) is responsible for
//! translating HTTP requests into these function calls and the resulting
//! [`crate::error::CoreError`] into HTTP status codes (see
//! [`crate::error::CoreError::http_status`]).

pub mod accounts;
pub mod combos;
pub mod models;
pub mod providers;

pub use accounts::*;
pub use combos::*;
pub use models::*;
pub use providers::*;

#[cfg(test)]
mod tests;
