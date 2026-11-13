//! Admission, serialization and fail-closed settlement for the non-chat
//! executors.
//!
//! The lifecycle tests drive the real gate against a real SQLite ledger and the
//! real usage journal: they assert what reaches the database (balance, one
//! `credits_debited` row, journal state) rather than the shape of the calls.
//! Every await that can block on a held lane is wrapped in a timeout so a
//! regression fails instead of hanging the suite.

use super::*;
use openproxy_pipeline::credits_gate::{CreditsAdmissionGate, PendingUnaryUsage};
use openproxy_types::ids::{ApiKeyId, ComboId, ComboTargetId, ProviderId};
use openproxy_types::{CoreError, EndpointKind};
use std::time::Duration;

/// Units of the seeded grant. One usage row priced at 1000 USD/1M tokens with
/// 1000 prompt tokens costs 1.00 USD = 1_000_000 micro-units, i.e. more than
/// this whole balance.
const GRANT_UNITS: i64 = 1_000;

/// Identity the gate keys its process-wide tables by: the shared writer Arc.
/// `Pipeline` computes the same value from `self.conn`, which is exactly what
/// `DbPool::writer_arc` hands out, so a chat request and a unary request for one
/// key contend on the same lane.
fn db_identity(pool: &DbPool) -> usize {
    std::sync::Arc::as_ptr(&pool.writer_arc()) as usize
}

/// Lanes and markers are process-wide and keyed by `(writer address, key id)`.
/// A finished test frees its pool, so the allocator can hand that address to a
/// later test; a key id that repeated would then share a lane with a dead test.
/// Every fixture therefore takes a fresh key id, making the pair unique for the
/// lifetime of the process.
static NEXT_KEY_ID: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1_000);

struct Fixture {
    pool: DbPool,
    key_id: i64,
    combo_id: i64,
    provider: ProviderId,
    model: String,
}

impl Fixture {
    /// `grant_units = None` seeds a key with no ledger rows: free/unlimited,
    /// never serialized.
    fn new(grant_units: Option<i64>) -> Self {
        let pool = DbPool::test_pool_with_prefix("openproxy-unary-credit").expect("test db pool");
        let key_id;
        let combo_id;
        {
            let conn = pool.writer();
            key_id = NEXT_KEY_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            conn.execute(
                "INSERT INTO api_keys (id, key_hash, label) VALUES (?1, ?2, 'unary-credit')",
                rusqlite::params![key_id, format!("unary-credit-fixture-{key_id}")],
            )
            .expect("insert api key");
            if let Some(units) = grant_units {
                conn.execute(
                    "INSERT INTO credit_transactions (api_key_id, kind, amount_units, remaining_units) \
                     VALUES (?1, 'grant', ?2, ?2)",
                    rusqlite::params![key_id, units],
                )
                .expect("grant credit");
            }
            conn.execute(
                "INSERT INTO combos (name, strategy) VALUES ('unary-credit-fixture', 'priority')",
                [],
            )
            .expect("insert combo");
            combo_id = conn
                .query_row("SELECT last_insert_rowid()", [], |row| row.get(0))
                .expect("combo id");
            // A combo price wins over model pricing, so the cost of a usage row
            // is fully determined by the token count below.
            conn.execute(
                "UPDATE combos SET pricing_input_per_1m = 1000.0, pricing_output_per_1m = 0.0 \
                 WHERE id = ?1",
                [combo_id],
            )
            .expect("price combo");
        }
        // The lane/marker tables are process-wide and keyed by an address that a
        // previous test's pool may have owned. Start each key clean so a stale
        // marker cannot fail an unrelated test.
        let gate = CreditsAdmissionGate::new();
        let db_id = db_identity(&pool);
        gate.clear_pending_debit(db_id, key_id);
        gate.clear_pending_unary_usage(db_id, key_id);
        Self {
            pool,
            key_id,
            combo_id,
            provider: ProviderId::new("mock-openai"),
            model: "unary-credit-model".to_string(),
        }
    }

    /// The key as the executors pass it (`Option<ApiKeyId>`), so call sites
    /// mirror the request path instead of special-casing the fixture.
    fn key(&self) -> Option<ApiKeyId> {
        (self.key_id > 0).then_some(ApiKeyId(self.key_id))
    }

    /// One billable usage row: `prompt_tokens` at 1000 USD per 1M tokens.
    fn args_with_tokens(&self, prompt_tokens: u32) -> UnaryUsageArgs<'_> {
        let mut args =
            UnaryUsageArgs::builder(&self.provider, &self.model, EndpointKind::Embedding)
                .api_key_id(self.key())
                .prompt_tokens(Some(prompt_tokens))
                .build();
        args.combo_id = Some(ComboId(self.combo_id));
        args
    }

    fn args(&self) -> UnaryUsageArgs<'_> {
        self.args_with_tokens(1000)
    }

    fn balance(&self) -> i64 {
        let conn = self.pool.reader();
        openproxy_db::credits_ledger::balance_units(&conn, self.key_id).expect("balance")
    }

    fn debited_rows(&self) -> i64 {
        let conn = self.pool.reader();
        conn.query_row(
            "SELECT COUNT(*) FROM usage WHERE api_key_id = ?1 AND credits_debited = 1",
            [self.key_id],
            |row| row.get(0),
        )
        .expect("count debits")
    }

    fn usage_rows(&self) -> i64 {
        let conn = self.pool.reader();
        conn.query_row(
            "SELECT COUNT(*) FROM usage WHERE api_key_id = ?1",
            [self.key_id],
            |row| row.get(0),
        )
        .expect("count usage")
    }

    fn pending(&self) -> Option<PendingUnaryUsage> {
        CreditsAdmissionGate::new().pending_unary_usage(db_identity(&self.pool), self.key_id)
    }

    /// Proof the lane is free right now, independent of any balance.
    async fn assert_lane_free(&self) {
        let gate = CreditsAdmissionGate::new();
        let guard = tokio::time::timeout(
            Duration::from_secs(2),
            gate.acquire(db_identity(&self.pool), self.key_id),
        )
        .await
        .expect("the key's lane is still held: a request leaked it");
        drop(guard);
    }

    fn writer_pragma(&self, sql: &str) {
        self.pool
            .writer()
            .execute_batch(sql)
            .expect("apply pragma to the writer");
    }
}

// ── admission ───────────────────────────────────────────────────────

#[tokio::test]
async fn free_key_admits_without_a_lane_and_keeps_concurrency() {
    let fixture = Fixture::new(None);
    let key = fixture.key();

    let (first, second) = tokio::join!(
        UnaryCreditLease::admit(&fixture.pool, key),
        UnaryCreditLease::admit(&fixture.pool, key)
    );
    first.expect("free key admits");
    second.expect("free key admits a second concurrent request");

    assert!(
        !CreditsAdmissionGate::new().has_lane(db_identity(&fixture.pool), fixture.key_id),
        "a key without ledger rows must never allocate a lane"
    );
}

#[tokio::test]
async fn chat_and_unary_contend_on_the_same_lane() {
    let fixture = Fixture::new(Some(GRANT_UNITS));
    let gate = CreditsAdmissionGate::new();
    let db_id = db_identity(&fixture.pool);

    // This is the exact lane a chat request holds from
    // `Pipeline::admit_credit_request` until its debit is posted.
    let chat_lane = gate.acquire(db_id, fixture.key_id).await;

    let unary = UnaryCreditLease::admit(&fixture.pool, fixture.key());
    let mut unary = std::pin::pin!(unary);
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut unary)
            .await
            .is_err(),
        "the unary executor must wait for the chat request's lane, not admit in parallel"
    );

    drop(chat_lane);
    let lease = tokio::time::timeout(Duration::from_secs(2), unary)
        .await
        .expect("the unary admission must proceed once the chat lane is released")
        .expect("admitted");
    drop(lease);
    fixture.assert_lane_free().await;
}

#[tokio::test]
async fn concurrent_unary_requests_cannot_spend_the_same_balance() {
    let fixture = Fixture::new(Some(GRANT_UNITS));
    let key = fixture.key();
    let mut lease = UnaryCreditLease::admit(&fixture.pool, key)
        .await
        .expect("first request is admitted");

    // The first request holds the lane while it "talks to the upstream", then
    // settles. The second one queues; whether it sees the spent balance is the
    // whole point.
    let first = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        lease
            .settle(&fixture.pool, &fixture.args())
            .await
            .expect("the first request's debit is persisted");
    };
    let second = async {
        let started = tokio::time::Instant::now();
        let admitted = UnaryCreditLease::admit(&fixture.pool, key).await;

        (started.elapsed(), admitted)
    };

    let ((), (waited, admitted)) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(first, second)
    })
    .await
    .expect("concurrent admission and settlement must finish");

    assert!(
        waited >= Duration::from_millis(80),
        "the second request was admitted without waiting for the first: {waited:?}"
    );
    let error = admitted.err().expect("the balance is spent");
    assert!(
        matches!(error, CoreError::InsufficientCredits { .. }),
        "expected an exhausted prepaid balance, got {error:?}"
    );
    assert_eq!(fixture.balance(), 0, "the grant is fully spent");
    assert_eq!(fixture.usage_rows(), 1, "only the admitted request records");
    assert_eq!(fixture.debited_rows(), 1, "only the admitted request bills");
    assert!(fixture.pending().is_none());
    fixture.assert_lane_free().await;
}

// ── settlement ──────────────────────────────────────────────────────

#[tokio::test]
async fn settle_persists_the_debit_before_releasing_the_lane() {
    let fixture = Fixture::new(Some(1_000_000));

    let mut lease = UnaryCreditLease::admit(&fixture.pool, fixture.key())
        .await
        .expect("admitted");
    // 250 prompt tokens at 1000 USD/1M = 0.25 USD = 250_000 micro-units.
    lease
        .settle(&fixture.pool, &fixture.args_with_tokens(250))
        .await
        .expect("settled");

    assert_eq!(fixture.balance(), 750_000, "the debit is already applied");
    assert_eq!(fixture.debited_rows(), 1);
    assert_eq!(fixture.usage_rows(), 1);
    assert!(fixture.pending().is_none());
    fixture.assert_lane_free().await;
}

#[tokio::test]
async fn settle_surfaces_a_failure_instead_of_answering_as_if_billed() {
    let fixture = Fixture::new(Some(GRANT_UNITS));
    let mut lease = UnaryCreditLease::admit(&fixture.pool, fixture.key())
        .await
        .expect("admitted");

    // Nothing can be written: the ledger must not learn that this request
    // succeeded, and the caller must not be told it was recorded.
    fixture.writer_pragma("PRAGMA query_only = ON");
    let error = lease
        .settle(&fixture.pool, &fixture.args())
        .await
        .expect_err("a failed persistence must be an error, not a log line");
    assert_eq!(
        error.http_status(),
        500,
        "expected an internal error, got {error:?}"
    );

    assert!(
        fixture.pending().is_some(),
        "the unposted charge must be retained for the key"
    );
    assert_eq!(fixture.usage_rows(), 0);
    assert_eq!(fixture.balance(), GRANT_UNITS, "nothing was billed");
    fixture.assert_lane_free().await;

    fixture.writer_pragma("PRAGMA query_only = OFF");
}

// ── journal / pending marker ────────────────────────────────────────

#[tokio::test]
async fn unposted_usage_denies_the_next_admission_until_it_is_retried() {
    let fixture = Fixture::new(Some(GRANT_UNITS));

    let mut lease = UnaryCreditLease::admit(&fixture.pool, fixture.key())
        .await
        .expect("admitted");
    fixture.writer_pragma("PRAGMA query_only = ON");
    lease
        .settle(&fixture.pool, &fixture.args())
        .await
        .expect_err("persistence must fail while the writer is read-only");
    assert!(fixture.pending().is_some());
    fixture.assert_lane_free().await;

    // Still failing: the key stays closed and is not admitted on a stale
    // balance that misses the lost charge.
    let denied = UnaryCreditLease::admit(&fixture.pool, fixture.key()).await;
    let error = denied.err().expect("the key must stay denied");
    assert_eq!(
        error.http_status(),
        500,
        "expected fail-closed 5xx: {error:?}"
    );
    assert!(
        fixture.pending().is_some(),
        "a failed retry must keep the marker"
    );
    assert_eq!(fixture.usage_rows(), 0, "no dispatch happened");

    // The writer recovers: the next admission re-appends the row, applies its
    // debit, clears the marker, and then finds nothing left to spend.
    fixture.writer_pragma("PRAGMA query_only = OFF");
    let denied = UnaryCreditLease::admit(&fixture.pool, fixture.key()).await;
    let error = denied.err().expect("the retried debit drains the balance");
    assert!(
        matches!(error, CoreError::InsufficientCredits { .. }),
        "the retried debit must be applied before the balance is read: {error:?}"
    );
    assert!(
        fixture.pending().is_none(),
        "the marker is cleared once durable"
    );
    assert_eq!(fixture.usage_rows(), 1, "the lost row landed exactly once");
    assert_eq!(fixture.debited_rows(), 1, "and it was charged exactly once");
    assert_eq!(fixture.balance(), 0);
    fixture.assert_lane_free().await;
}

#[tokio::test]
async fn journal_drain_failure_denies_admission_on_a_stale_balance() {
    let fixture = Fixture::new(Some(GRANT_UNITS));

    // A durable debit nobody applied yet (for example a settlement task that
    // was aborted). The admission must not read the balance before it lands.
    let job = openproxy_pipeline::worker::BackgroundJob::MarkClientResponse {
        request_id: "unary-credit-journal".into(),
        attempt: 1,
        target_id: ComboTargetId(1),
    };
    openproxy_pipeline::worker::admit_job(&fixture.pool.writer_arc(), &job)
        .expect("journal a pending job");
    fixture.writer_pragma("PRAGMA query_only = ON");

    let denied = UnaryCreditLease::admit(&fixture.pool, fixture.key()).await;
    let error = denied
        .err()
        .expect("an undrainable journal must deny the request");
    assert_eq!(
        error.http_status(),
        500,
        "a stale balance must fail closed, got {error:?}"
    );
    fixture.assert_lane_free().await;

    fixture.writer_pragma("PRAGMA query_only = OFF");
    openproxy_pipeline::worker::replay_pending(&fixture.pool.writer_arc())
        .expect("drain once the writer is writable again");
    let lease = UnaryCreditLease::admit(&fixture.pool, fixture.key())
        .await
        .expect("admitted once the journal is drained");
    drop(lease);
}

// ── cancellation ────────────────────────────────────────────────────

#[tokio::test]
async fn cancelled_unary_request_releases_the_lane() {
    let fixture = Fixture::new(Some(GRANT_UNITS));
    let gate = CreditsAdmissionGate::new();
    let db_id = db_identity(&fixture.pool);
    let pool = fixture.pool.clone();
    let key = fixture.key();

    // Stands in for an executor that is aborted mid-flight (client disconnect,
    // shutdown): only the lease is dropped, no settlement runs.
    let task = tokio::spawn(async move {
        let _lease = UnaryCreditLease::admit(&pool, key).await.expect("admitted");
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        gate.has_lane(db_id, fixture.key_id),
        "the request holds the lane"
    );

    task.abort();
    let _ = task.await;

    let guard = tokio::time::timeout(Duration::from_secs(2), gate.acquire(db_id, fixture.key_id))
        .await
        .expect("an aborted unary request leaked its lane");
    drop(guard);
    assert_eq!(fixture.balance(), GRANT_UNITS, "nothing was billed");
    assert_eq!(fixture.usage_rows(), 0);
}

#[tokio::test]
async fn cancelled_waiter_does_not_wedge_the_key() {
    let fixture = Fixture::new(Some(GRANT_UNITS));
    let gate = CreditsAdmissionGate::new();
    let db_id = db_identity(&fixture.pool);
    let pool = fixture.pool.clone();
    let key = fixture.key();

    let held = gate.acquire(db_id, fixture.key_id).await;
    let waiter = tokio::spawn(async move { UnaryCreditLease::admit(&pool, key).await.map(drop) });
    tokio::time::sleep(Duration::from_millis(50)).await;
    // The queued request disappears before it is admitted (client went away).
    waiter.abort();
    let _ = waiter.await;
    drop(held);

    let lease = tokio::time::timeout(
        Duration::from_secs(2),
        UnaryCreditLease::admit(&fixture.pool, fixture.key()),
    )
    .await
    .expect("a cancelled waiter must not block the lane queue")
    .expect("admitted");
    drop(lease);
    fixture.assert_lane_free().await;
}
