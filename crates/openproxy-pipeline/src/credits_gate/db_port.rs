//! Database port of the admission policy.
//!
//! Implemented for [`Pipeline`] (chat, which may run without a pool) and for a
//! bare [`DbPool`] (the unary executors). Both entry points run
//! `super::admit_prepaid`, so the fail-closed ordering exists once instead of
//! once per crate.

use super::{PendingDebit, PendingUnaryUsage, persist_unary_usage};
use crate::Pipeline;
use openproxy_db::DbPool;
use openproxy_types::error::CoreError;
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::Arc;

/// Database side of the admission policy.
pub(super) trait CreditGateDb {
    /// Identity of the shared writer, i.e. the database half of the lane key.
    fn db_id(&self) -> usize;
    /// The shared writer handle, used by the durable intent bookkeeping.
    fn writer_arc(&self) -> Arc<parking_lot::Mutex<Connection>>;
    /// On-disk database file, so the per-key lock files live next to it.
    async fn database_path(&self) -> openproxy_types::Result<PathBuf>;
    async fn credit_based(&self, key: i64) -> openproxy_types::Result<bool>;
    async fn balance(&self, key: i64) -> openproxy_types::Result<i64>;
    async fn post_chat_debit(&self, debit: &PendingDebit) -> openproxy_types::Result<()>;
    async fn post_unary_usage(
        &self,
        pending: &PendingUnaryUsage,
    ) -> std::result::Result<(), UnaryPersistError>;
    async fn drain_journal(&self) -> openproxy_types::Result<()>;
}

/// Why a unary usage persistence attempt failed, classified by whether a
/// durable copy of the charge survived.
pub(super) enum UnaryPersistError {
    /// The journal append never committed: nothing was written anywhere, so a
    /// later admission must re-append this usage row.
    Lost(CoreError),
    /// The journal append committed and only applying it failed. The durable
    /// entry is applied by any later drain, so no marker is needed.
    Journaled(CoreError),
}

impl CreditGateDb for Pipeline {
    fn db_id(&self) -> usize {
        self.credit_db_id()
    }

    fn writer_arc(&self) -> Arc<parking_lot::Mutex<Connection>> {
        Arc::clone(&self.conn)
    }

    /// A `Pipeline` built with a pool already knows the file. The bare-connection
    /// variant asks SQLite for the `main` file instead of guessing.
    async fn database_path(&self) -> openproxy_types::Result<PathBuf> {
        if let Some(pool) = &self.db_pool {
            return Ok(pool.path().to_path_buf());
        }
        self.read_db(|conn| {
            conn.query_row(
                "SELECT file FROM pragma_database_list WHERE name = 'main'",
                [],
                |row| row.get::<_, String>(0),
            )
            .map(PathBuf::from)
            .map_err(|error| {
                CoreError::Internal(format!("credit gate: database path lookup failed: {error}"))
            })
        })
        .await
    }

    async fn credit_based(&self, key: i64) -> openproxy_types::Result<bool> {
        self.read_db(move |conn| openproxy_db::credits_ledger::is_credit_based(conn, key))
            .await
    }

    async fn balance(&self, key: i64) -> openproxy_types::Result<i64> {
        self.read_db(move |conn| openproxy_db::credits_ledger::balance_units(conn, key))
            .await
    }

    async fn post_chat_debit(&self, debit: &PendingDebit) -> openproxy_types::Result<()> {
        self.post_debit(debit).await
    }

    async fn post_unary_usage(
        &self,
        pending: &PendingUnaryUsage,
    ) -> std::result::Result<(), UnaryPersistError> {
        let conn = Arc::clone(&self.conn);
        let usage = pending.usage.clone();
        match tokio::task::spawn_blocking(move || persist_unary_usage(&conn, &usage)).await {
            Ok(result) => result,
            Err(error) => Err(UnaryPersistError::Lost(CoreError::Internal(format!(
                "unary usage persistence task failed to run: {error}"
            )))),
        }
    }

    async fn drain_journal(&self) -> openproxy_types::Result<()> {
        self.drain_usage_journal().await
    }
}

impl CreditGateDb for DbPool {
    fn db_id(&self) -> usize {
        // `writer_arc` clones the handle; the allocation it points at is the
        // one every `Pipeline` built from this pool carries, which is exactly
        // what makes a chat request and a unary request share one lane.
        Arc::as_ptr(&self.writer_arc()) as usize
    }

    fn writer_arc(&self) -> Arc<parking_lot::Mutex<Connection>> {
        DbPool::writer_arc(self)
    }

    async fn database_path(&self) -> openproxy_types::Result<PathBuf> {
        Ok(self.path().to_path_buf())
    }

    async fn credit_based(&self, key: i64) -> openproxy_types::Result<bool> {
        self.spawn_read(move |conn| openproxy_db::credits_ledger::is_credit_based(conn, key))
            .await
    }

    async fn balance(&self, key: i64) -> openproxy_types::Result<i64> {
        self.spawn_read(move |conn| openproxy_db::credits_ledger::balance_units(conn, key))
            .await
    }

    async fn post_chat_debit(&self, debit: &PendingDebit) -> openproxy_types::Result<()> {
        let conn = self.writer_arc();
        let request_id = debit.request_id.clone();
        let attempt = debit.attempt;
        let target_id = debit.target_id;
        match tokio::task::spawn_blocking(move || {
            openproxy_db::cost::mark_winner_usage_row(&conn.lock(), &request_id, attempt, target_id)
        })
        .await
        {
            Ok(result) => result,
            Err(error) => Err(CoreError::Internal(format!(
                "credit debit post failed to run: {error}"
            ))),
        }
    }

    async fn post_unary_usage(
        &self,
        pending: &PendingUnaryUsage,
    ) -> std::result::Result<(), UnaryPersistError> {
        let conn = self.writer_arc();
        let usage = pending.usage.clone();
        match tokio::task::spawn_blocking(move || persist_unary_usage(&conn, &usage)).await {
            Ok(result) => result,
            Err(error) => Err(UnaryPersistError::Lost(CoreError::Internal(format!(
                "unary usage persistence task failed to run: {error}"
            )))),
        }
    }

    async fn drain_journal(&self) -> openproxy_types::Result<()> {
        let conn = self.writer_arc();
        match tokio::task::spawn_blocking(move || crate::worker::replay_pending(&conn)).await {
            Ok(result) => result,
            Err(error) => Err(CoreError::Internal(format!(
                "credit journal drain failed to run: {error}"
            ))),
        }
    }
}
