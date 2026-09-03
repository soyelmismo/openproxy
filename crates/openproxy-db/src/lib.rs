#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod app_config;
pub mod batch;
pub mod conn;
pub mod crud;
pub mod migrations;

pub mod accounts;
pub mod cooldowns;
pub mod cost;
pub mod pricing;
pub mod providers;
pub mod secrets;

pub use batch::{
    DEFAULT_CHUNK_SIZE, SQLITE_MAX_VARIABLE_NUMBER, batch_insert, in_placeholders, query_in_chunks,
    query_in_chunks_by, query_in_chunks_by_with_params, query_in_chunks_with_params,
    values_placeholders,
};
pub use conn::{ArcReaderGuard, ArcWriterGuard, DbPool, ReaderGuard, WriterGuard};
pub use crud::FromRow;
pub use secrets::MasterKey;
pub mod combos;
pub mod error;
pub mod free_proxies;
pub mod live_limited;
pub mod maintenance;
pub mod models;
pub mod notifications;

pub use error::{DbErrorKind, classify_sqlite_error, map_constraint_error};
pub use maintenance::DbTable;

#[cfg(test)]
pub(crate) mod test_db;
