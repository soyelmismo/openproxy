#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

#[macro_use]
pub mod macros;
pub mod app_config;
pub mod batch;
pub mod conn;
pub mod crud;
pub mod migrations;

pub mod accounts;
pub mod cooldowns;
pub mod cost;
// The payments subsystem. OFF in the public build: the money modules are not
// compiled at all and `credits_ledger` becomes a stub whose `is_credit_based`
// is always false, so every admission/ledger call site no-ops without the
// credit tables even existing (their migrations are not embedded either).
mod credits_billing;
#[cfg(feature = "payments")]
pub mod credits_ledger;
#[cfg(all(test, feature = "payments"))]
mod credits_ledger_tests;
#[cfg(not(feature = "payments"))]
pub mod credits_ledger {
    //! Payments-disabled build. Same surface the admission paths call; every
    //! answer says "no credits exist", which is exactly true without payments.
    use openproxy_types::Result;
    use rusqlite::Connection;

    /// No key is credit-based: the balance system never applies.
    pub fn is_credit_based(_conn: &Connection, _api_key_id: i64) -> Result<bool> {
        Ok(false)
    }

    /// Never consulted when [`is_credit_based`] answers false; a huge balance
    /// keeps the type honest without a money table.
    pub fn balance_units(_conn: &Connection, _api_key_id: i64) -> Result<i64> {
        Ok(i64::MAX)
    }

    /// Spending without payments is never recorded (usage journal still is).
    pub fn record_spend(_conn: &Connection, _api_key_id: i64, _cost_units: i64) -> Result<i64> {
        Ok(0)
    }

    /// Nothing to expire without credit lots.
    pub fn expire_due(_conn: &Connection) -> Result<usize> {
        Ok(0)
    }
}
pub mod pricing;
pub mod providers;
// Durable per-request credit intents. OFF in the public build: no reservation
// table exists, so the credit gate never holds or checkpoints (no key is
// credit-based). Same surface the pipeline calls, always answering "nothing
// to reserve".
#[cfg(not(feature = "payments"))]
pub mod reservations {
    use openproxy_types::{CoreError, Result};
    use rusqlite::Connection;

    pub const STATE_HELD: &str = "held";
    pub const STATE_SETTLED: &str = "settled";
    pub const STATE_RELEASED: &str = "released";
    pub const OUTCOME_BILLED: &str = "billed";
    pub const OUTCOME_UNBILLED: &str = "unbilled";
    pub const OUTCOME_RECOVERED_BILLED: &str = "recovered_billed";
    pub const OUTCOME_RECOVERED_PARTIAL: &str = "recovered_partial";
    pub const OUTCOME_RECOVERED_UNBILLED: &str = "recovered_unbilled";

    /// Mirror of the real shape: the credit gate's recovery code only reads
    /// these fields, and without payments no row can ever be returned.
    #[derive(Clone, Debug)]
    pub struct OpenReservation {
        pub id: i64,
        pub request_id: String,
        pub attempt: u8,
        pub reserved_units: i64,
        pub delivery_started: bool,
        pub generated_units: i64,
        pub created_at: String,
    }

    /// Mirror of the real shape: what the durable `usage` table proves.
    #[derive(Clone, Debug)]
    pub struct UsageEvidence {
        pub row_id: i64,
        pub attempt: u8,
        pub target_id: Option<i64>,
        pub cost_units: i64,
        pub debited: bool,
        pub client_response: bool,
    }

    /// Never called in practice without payments (no key is credit-based),
    /// but keeps the credit gate's type surface intact.
    pub fn hold(
        _conn: &Connection,
        _api_key_id: i64,
        _request_id: &str,
        _attempt: u8,
        _reserved_units: i64,
        _quoted: bool,
    ) -> Result<i64> {
        Ok(0)
    }

    /// No reservation table exists: no intent can be open, so nothing is
    /// listed for reconciliation.
    pub fn open(_conn: &Connection, _api_key_id: i64) -> Result<Vec<OpenReservation>> {
        Ok(Vec::new())
    }

    /// Nothing to close without a reservation table.
    pub fn close(
        _conn: &Connection,
        _id: i64,
        _state: &str,
        _outcome: &str,
        _charged_units: i64,
        _note: &str,
    ) -> Result<bool> {
        Ok(false)
    }

    /// Nothing to mark without a reservation table.
    pub fn mark_delivery_started(
        _conn: &Connection,
        _request_id: &str,
        _attempt: u8,
    ) -> Result<()> {
        Ok(())
    }

    /// No open intent can exist, so nothing is ever checkpointed; a real
    /// `false` keeps the caller's "intent no longer open" path honest.
    pub fn checkpoint(
        _conn: &Connection,
        _api_key_id: i64,
        _request_id: &str,
        _attempt: u8,
        _delivered_units: i64,
    ) -> Result<bool> {
        Ok(false)
    }

    /// No `usage` evidence can exist for a hold that was never opened.
    pub fn delivered_usage(
        _conn: &Connection,
        _request_id: &str,
        _api_key_id: i64,
    ) -> Result<Option<UsageEvidence>> {
        Ok(None)
    }

    /// Zero: no reservation rows exist.
    pub fn reserved_units(_conn: &Connection, _api_key_id: i64) -> Result<i64> {
        Ok(0)
    }

    /// Same error shape as the real one: an intent failure stays an internal
    /// error naming the missing recording.
    pub fn hold_blocked(error: &CoreError) -> CoreError {
        CoreError::Internal(format!(
            "credit ledger: could not record the request intent before dispatch: {error}"
        ))
    }
}
#[cfg(feature = "payments")]
pub mod reservations;
pub mod secrets;
#[cfg(feature = "sql-profile")]
pub mod sql_profile;
pub mod usage_journal;
pub mod usage_writer;

pub use batch::{
    DEFAULT_CHUNK_SIZE, SQLITE_MAX_VARIABLE_NUMBER, batch_insert, in_placeholders, query_in_chunks,
    query_in_chunks_by, query_in_chunks_by_with_params, query_in_chunks_with_params,
    values_placeholders,
};
pub use conn::{
    ArcReaderGuard, ArcWriterGuard, DbPool, ReaderGuard, WriterGuard, routing_config_epoch,
    routing_cooldown_epoch,
};
pub use crud::FromRow;
pub use secrets::MasterKey;
pub mod combos;
pub mod error;
pub mod free_proxies;
pub mod live_limited;
pub mod maintenance;
pub mod models;
pub mod notifications;

pub use error::{
    BUSY_RETRY_DELAYS, DbErrorKind, classify_sqlite_error, is_sqlite_busy, map_constraint_error,
    with_busy_retry,
};
pub use maintenance::DbTable;
pub mod testing;
pub use testing as test_db;
