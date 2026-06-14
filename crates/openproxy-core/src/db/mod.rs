//! SQLite persistence layer.
//!
//! See docs/mvp-spec.md §8 (schema) and §9 (migration strategy).

pub mod conn;
pub mod migrations;

pub use conn::DbPool;
