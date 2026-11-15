//! Per-key admission gate for prepaid credit keys, fail-closed.
//!
//! A prepaid key is billed **after** the response: the usage row is recorded
//! (journaled) and only later marked billable, which is what applies the ledger
//! debit. Without serialization, N concurrent requests of the same key all read
//! `balance > 0` during admission, all dispatch, and the last debits are
//! clamped to whatever is left (`debit = min(cost, balance)`), so the key spends
//! credit it does not have.
//!
//! Two things enforce the invariant **"a spend is only admitted on a balance
//! that already includes every debit this process owes"**:
//!
//! 1. A per-key async lane held from admission until the debit of that request
//!    is posted, so a queued request reads the balance only after the previous
//!    one settled.
//! 2. A per-key *pending debit* marker. If settlement cannot post the charge
//!    (DB error, journal capacity, join failure), the lane is released but the
//!    charge stays recorded here; the next admission for that key retries the
//!    post and is **denied** until it succeeds. Releasing the lane matters:
//!    parking the mutex would hang the key forever, while the marker turns the
//!    same condition into a clean error.
//!
//! Admission itself is fail-closed: a journal drain that fails means the
//! balance would be stale, so the request is denied instead of being allowed to
//! spend against a number the ledger cannot vouch for.
//!
//! The lane and pending tables are **process-wide**, keyed by
//! `(database identity, key id)`, not stored per `Pipeline`: the server builds a
//! fresh `Pipeline` for every HTTP request (`PipelineRunner::build_pipeline`),
//! so an instance-local table would never serialize two real requests. The
//! database identity is the address of the shared writer `Arc`, which every
//! `Pipeline` cloned from the same `AppState` pool shares.
//!
//! The non-chat executors (audio, embeddings, images, systemone) never go
//! through `Pipeline::run`, so they call [`admit_unary_credit`] before
//! dispatching and [`settle_unary_credit`] once their usage row is persisted.
//! Both those entry points and `Pipeline::admit_credit_request` run the same
//! [`admit_prepaid`] policy against the same lane table, so a unary request and
//! a chat request of one key serialize against each other instead of each
//! having its own guard.
//!
//! What this does **not** promise:
//!
//! - Only keys with at least one ledger row are governed (`is_credit_based`).
//!   Free/unlimited keys never allocate a lane and keep full concurrency.
//! - A single request may still exceed the balance: its cost is only known after
//!   the upstream answers, and the debit is clamped. The gate stops *concurrent*
//!   leakage, not a strict monetary bound; callers that know a per-request
//!   maximum can pass it as the `budget` of [`Pipeline::admit_credit_request_with_budget`]
//!   (or [`admit_unary_credit_with_budget`]) and enforce it at dispatch.
//!
//! ## Across processes
//!
//! Two things extend the same invariant to several server processes sharing one
//! database:
//!
//! 1. A per-key advisory **file lock** next to the database file
//!    ([`interprocess`]), taken after the in-process lane and held exactly as
//!    long. Admission, reconciliation, the balance read and the settlement of
//!    that request all happen under it. The OS releases it when the process
//!    dies, so it cannot go stale: no lease to expire, nothing to unlock by hand.
//! 2. A durable **intent row** (`openproxy_db::reservations`) committed before
//!    anything is dispatched. The in-memory pending markers die with the
//!    process; the row does not. Because at most one live process owns an open
//!    intent for a key, every open intent observed while holding the lock
//!    belongs to a request whose owner is gone, and the next admission resolves
//!    it from durable `usage` evidence before reading the balance: a delivered
//!    row is debited idempotently, a started stream with an explicit quote is
//!    charged that quote, and anything else closes without a charge. Recovery is
//!    automatic on the next admission of that key — never a manual unlock.
//!
//! A debit whose post fails and whose journal entry is also lost leaves both the
//! in-memory marker and the durable intent in place; the key is denied (never
//! silently allowed) until a later admission can post it.
//!
//! No `rusqlite` guard is ever held across an `await`: journal drains and
//! balance reads run inside `spawn_blocking` / the pool's readers.

use crate::{ErrorPhase, Pipeline, PipelineRequest, PipelineResult};
use dashmap::DashMap;
use openproxy_db::DbPool;
use openproxy_types::UsageInput;
use openproxy_types::error::CoreError;
use openproxy_types::ids::{ApiKeyId, ComboTargetId, RequestId};
use std::sync::{Arc, LazyLock};
use tokio::sync::{Mutex, OwnedMutexGuard};

mod db_port;
mod interprocess;
mod reservation;
mod unary_persist;

use db_port::{CreditGateDb, UnaryPersistError};

pub use reservation::note_delivery_started;
use unary_persist::persist_unary_usage;

// Seeds credit_reservations rows directly, which only exist with payments.
#[cfg(all(test, feature = "payments"))]
mod interprocess_tests;

/// Lanes kept before idle ones are pruned. One lane per (database, prepaid key);
/// the map is pruned opportunistically so a long-lived process does not grow
/// one entry per key ever seen.
const LANE_PRUNE_THRESHOLD: usize = 1024;

/// `(database identity, api key id)`: the resource whose spend must be
/// serialized.
type LaneKey = (usize, i64);
type LaneMap = DashMap<LaneKey, Arc<Mutex<()>>>;
type PendingMap = DashMap<LaneKey, PendingDebit>;
type PendingUnaryMap = DashMap<LaneKey, PendingUnaryUsage>;

/// Lanes are process-wide on purpose: see the module note about per-request
/// `Pipeline` construction.
static LANES: LazyLock<LaneMap> = LazyLock::new(DashMap::new);
/// Charges this process owes but has not managed to post.
static PENDING_DEBITS: LazyLock<PendingMap> = LazyLock::new(DashMap::new);
/// Unary usage rows whose journal admission never committed, so the charge they
/// carry has no durable copy anywhere.
static PENDING_UNARY: LazyLock<PendingUnaryMap> = LazyLock::new(DashMap::new);

/// A billable usage row whose ledger debit did not commit.
///
/// It carries exactly what the idempotent repo call needs
/// (`mark_winner_usage_row` marks the row and applies the debit in one
/// transaction, and the debit is marker-guarded), so the retry never double
/// charges.
#[derive(Clone, Debug)]
pub struct PendingDebit {
    request_id: String,
    attempt: u8,
    target_id: ComboTargetId,
}

impl PendingDebit {
    #[must_use]
    pub fn new(request_id: impl Into<String>, attempt: u8, target_id: ComboTargetId) -> Self {
        Self {
            request_id: request_id.into(),
            attempt,
            target_id,
        }
    }
}

/// A unary usage row whose journal append never committed.
///
/// It carries the whole [`UsageInput`] because the retry re-appends the exact
/// same `RecordUsage` job: the append is atomic (it commits or writes nothing),
/// and the charge it applies is the row's own `cost_usd`, so replaying it once
/// is the same debit the original request owed. The retry first checks whether
/// a row with the same `(request_id, attempt)` already landed, so an ambiguous
/// failure (a panic between append and apply) cannot double-insert or
/// double-charge.
#[derive(Clone, Debug)]
pub struct PendingUnaryUsage {
    usage: UsageInput,
}

impl PendingUnaryUsage {
    #[must_use]
    pub fn new(usage: UsageInput) -> Self {
        Self { usage }
    }
}

/// Fair per-key lanes plus the pending-debit markers. `tokio::sync::Mutex`
/// queues waiters FIFO and its owned guard releases on drop, so a cancelled or
/// panicked request cannot leave a key blocked.
#[derive(Default)]
pub struct CreditsAdmissionGate;

impl CreditsAdmissionGate {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Number of live lanes in this process. Diagnostic only: it also counts
    /// lanes of other keys and other databases.
    #[must_use]
    pub fn active_lanes(&self) -> usize {
        LANES.len()
    }

    /// Whether a lane currently exists for this key. Test hook: a key without
    /// credit rows must never appear here.
    #[must_use]
    pub fn has_lane(&self, db_id: usize, key_id: i64) -> bool {
        LANES.contains_key(&(db_id, key_id))
    }

    /// A charge for this key that is owed but not posted.
    #[must_use]
    pub fn pending_debit(&self, db_id: usize, key_id: i64) -> Option<PendingDebit> {
        PENDING_DEBITS
            .get(&(db_id, key_id))
            .map(|entry| entry.value().clone())
    }

    /// Record a charge that must be posted before this key may spend again.
    pub fn note_pending_debit(&self, db_id: usize, key_id: i64, debit: PendingDebit) {
        PENDING_DEBITS.insert((db_id, key_id), debit);
    }

    /// Forget the pending charge once it is durably applied.
    pub fn clear_pending_debit(&self, db_id: usize, key_id: i64) {
        PENDING_DEBITS.remove(&(db_id, key_id));
    }

    /// A unary usage row this key still owes, if its journal append was lost.
    #[must_use]
    pub fn pending_unary_usage(&self, db_id: usize, key_id: i64) -> Option<PendingUnaryUsage> {
        PENDING_UNARY
            .get(&(db_id, key_id))
            .map(|entry| entry.value().clone())
    }

    /// Record a unary usage row that must be journalled before this key may
    /// spend again.
    pub fn note_pending_unary_usage(&self, db_id: usize, key_id: i64, pending: PendingUnaryUsage) {
        PENDING_UNARY.insert((db_id, key_id), pending);
    }

    /// Forget the unary usage row once it is durable (or applied).
    pub fn clear_pending_unary_usage(&self, db_id: usize, key_id: i64) {
        PENDING_UNARY.remove(&(db_id, key_id));
    }

    /// Wait for this key's turn. The returned guard owns the lane and releases
    /// it on drop.
    ///
    /// This is the in-process half only. Admission takes
    /// [`CreditsAdmissionGate::acquire_leased`], which adds the cross-process
    /// file lock on top; calling this directly is correct only for tests and for
    /// callers that already hold the file lock.
    pub async fn acquire(&self, db_id: usize, key_id: i64) -> CreditsLaneGuard {
        self.prune_idle_lanes();
        // Take the Arc out of the map before awaiting: a DashMap entry guard
        // held across an `await` would stall every key hashing to that shard.
        let lane = {
            let entry = LANES
                .entry((db_id, key_id))
                .or_insert_with(|| Arc::new(Mutex::new(())));
            Arc::clone(entry.value())
        };
        let guard = lane.lock_owned().await;
        CreditsLaneGuard {
            db_id,
            key_id,
            budget_units: 0,
            reservation_id: None,
            _lane: guard,
            lease: None,
        }
    }

    /// Drop lanes nobody holds. A lane with a single strong reference is only
    /// reachable from the map, so no request is queued on it.
    ///
    /// Pending markers are deliberately *not* pruned: they are the safety
    /// valve, not a cache.
    fn prune_idle_lanes(&self) {
        if LANES.len() < LANE_PRUNE_THRESHOLD {
            return;
        }
        LANES.retain(|_, lane| Arc::strong_count(lane) > 1);
    }
}

/// One key's exclusive turn: admission → dispatch → posted debit.
///
/// Holds three things that must all outlive the request body: the in-process
/// lane, the cross-process file lock (when the guard was taken through
/// [`CreditsAdmissionGate::acquire_leased`]) and the id of the durable intent
/// row written before dispatch. `Drop` releases the locks; the intent row is
/// closed by settlement and, if that never runs, reconciled by the next
/// admission.
pub struct CreditsLaneGuard {
    db_id: usize,
    key_id: i64,
    /// Budget this request was admitted under, in micro-USDC units: the whole
    /// available balance when the caller supplied no quote. Captured from the
    /// authoritative balance read taken under both locks.
    budget_units: i64,
    reservation_id: Option<i64>,
    _lane: OwnedMutexGuard<()>,
    lease: Option<interprocess::InterprocessLease>,
}

impl CreditsLaneGuard {
    #[must_use]
    pub fn key_id(&self) -> i64 {
        self.key_id
    }

    #[must_use]
    pub fn db_id(&self) -> usize {
        self.db_id
    }

    /// Maximum spend this request was admitted against. Callers that can bound
    /// the cost of one request use it as the dispatch budget: a delivered
    /// response costing more is exactly the overrun this gate cannot prevent by
    /// serializing alone.
    #[must_use]
    pub fn budget_units(&self) -> i64 {
        self.budget_units
    }

    /// Durable intent row this guard owns, if the database recorded one.
    #[must_use]
    pub fn reservation_id(&self) -> Option<i64> {
        self.reservation_id
    }

    /// Test hook: proves admission really took the cross-process lock.
    #[cfg(all(test, feature = "payments"))]
    #[must_use]
    pub(crate) fn has_interprocess_lease(&self) -> bool {
        self.lease.is_some()
    }

    pub(crate) fn set_budget_units(&mut self, budget_units: i64) {
        self.budget_units = budget_units;
    }

    pub(crate) fn set_reservation(&mut self, reservation_id: i64) {
        self.reservation_id = Some(reservation_id);
    }

    fn with_lease(mut self, lease: interprocess::InterprocessLease) -> Self {
        self.lease = Some(lease);
        self
    }
}

/// Result of the admission step for one request.
pub enum CreditAdmission {
    /// Proceed. `Some` carries the lane that must be settled (and only then
    /// released); `None` means the request is not governed by prepaid credit.
    Open(Option<CreditsLaneGuard>),
    /// Reject before dispatch (balance exhausted while queued, an unposted
    /// charge, or a balance that cannot be trusted). The lane, if taken, is
    /// already released. Boxed because `PipelineResult` is far larger than the
    /// other variant and this value travels through the request path.
    Denied(Box<PipelineResult>),
}

impl Pipeline {
    /// Identity of the database this pipeline spends from, as far as the lane
    /// table is concerned: the shared writer connection. Stable for every
    /// `Pipeline` built from the same pool.
    fn credit_db_id(&self) -> usize {
        std::sync::Arc::as_ptr(&self.conn) as usize
    }

    /// Admit one request under the prepaid-credit rules (see [`admit_prepaid`]
    /// for the policy). The lane travels back to `Pipeline::run`, which settles
    /// it after the debit is persisted.
    pub(crate) async fn admit_credit_request(&self, req: &PipelineRequest) -> CreditAdmission {
        self.admit_credit_request_with_budget(req, None).await
    }

    /// Same admission, with an explicit maximum-cost quote for this request.
    ///
    /// The quote becomes the durable intent's reserved amount, so a crash that
    /// leaves a started stream without its usage row can be reconciled against
    /// a number the caller agreed to instead of the whole balance. Callers that
    /// can bound the cost of the request should always pass one; the gate
    /// records it and exposes it back through [`CreditsLaneGuard::budget_units`].
    pub(crate) async fn admit_credit_request_with_budget(
        &self,
        req: &PipelineRequest,
        budget_units: Option<i64>,
    ) -> CreditAdmission {
        let request_id = req.request_id.to_string();
        match admit_prepaid(self, req.api_key_id, request_id, 1, budget_units).await {
            Ok(lane) => CreditAdmission::Open(lane),
            Err(error) => {
                CreditAdmission::Denied(Box::new(Self::failure(error, 1, ErrorPhase::Route)))
            }
        }
    }

    /// Post this request's debit — or record that it is still owed — then
    /// release the key lane.
    ///
    /// The lane is released on both outcomes. Safety comes from the pending
    /// marker, not from the lock: a held lock would hang the key, while the
    /// marker makes the next admission retry the post and refuse the spend.
    pub(crate) async fn settle_credit_request(
        &self,
        lane: CreditsLaneGuard,
        result: &PipelineResult,
    ) {
        let db = lane.db_id();
        let key = lane.key_id();

        // Only a delivered response or a client cancel is billable. Failure
        // results also carry a `usage_tuple` (the tracker records the failed
        // attempt), and marking one of those would flip `client_response` on
        // an error row and charge the customer for a request the upstream
        // never answered.
        //
        // Client cancels (499) ARE billable since the antseed-cancel policy:
        // the seller never aborts its upstream, so a cut stream still costs us
        // full input + full generation via the seller's NeedAuth claim. The
        // user pays the in-flight delivered tokens + full prompt at the
        // uncached input rate (cost.rs bills 499 with cached_tokens = None).
        // A cancel before dispatch records no usage row at all, so nothing is
        // charged for it.
        let delivered = result.error.is_none() && (200..300).contains(&result.status_code);
        let client_cancelled = matches!(
            result.error.as_ref(),
            Some(CoreError::Cancelled(
                openproxy_types::CancelReason::ClientDisconnected
            ))
        );
        let debit = result
            .usage_tuple
            .filter(|_| delivered || client_cancelled)
            .map(|(request_id, attempt, target_id)| {
                PendingDebit::new(request_id.to_string(), attempt, target_id)
            });

        let mut posted = true;
        let usage_backed = debit.is_some();
        if let Some(pending) = debit.as_ref() {
            // The normal debit path is the journal's `MarkClientResponse` job,
            // which the drain below applies. Re-issuing the same idempotent mark
            // here also covers a request whose job never reached the journal
            // (worker channel closed, journal full). Both paths set
            // `client_response`/`was_winner`, and `debit_usage` is
            // marker-guarded, so this can never double-charge.
            if let Err(error) = self.post_debit(pending).await {
                tracing::warn!(
                    %error,
                    api_key_id = key,
                    "credit settlement: debit not posted; keeping it pending"
                );
                posted = false;
            }
        }
        if let Err(error) = self.drain_usage_journal().await {
            tracing::warn!(
                %error,
                api_key_id = key,
                "credit settlement: journal drain failed; keeping the debit pending"
            );
            posted = false;
        }

        // A usage tuple identifies an attempted write, not durable evidence.
        // A lost 499 row must fall back to its delivered-output checkpoint.
        let mut usage_backed = usage_backed;
        if posted
            && client_cancelled
            && let Some(pending) = debit.as_ref()
        {
            let request_id = pending.request_id.clone();
            let attempt = pending.attempt;
            let target_id = pending.target_id;
            match self
                .read_db(move |conn| {
                    conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM usage WHERE request_id = ?1 \
                     AND attempt = ?2 AND combo_target_id = ?3 AND credits_debited = 1)",
                        rusqlite::params![request_id, attempt, target_id.0],
                        |row| row.get::<_, bool>(0),
                    )
                    .map_err(openproxy_db::error::map_db_error)
                })
                .await
            {
                Ok(recorded) => usage_backed = recorded,
                Err(error) => {
                    tracing::warn!(%error, "credit settlement: cancel debit evidence unavailable");
                    posted = false;
                }
            }
        }

        if posted {
            self.credits_gate.clear_pending_debit(db, key);
        } else if let Some(pending) = debit {
            // Net fail-closed: the next admission for this key retries the post
            // and is denied until it lands.
            self.credits_gate.note_pending_debit(db, key, pending);
        }

        // Close the durable intent only once its charge is durable. Two cases
        // stay open on purpose and are reconciled by the next admission of
        // this key:
        // - a charge that could not be posted yet (pending marker, failed
        //   drain), and
        // - a delivered response whose usage row failed to record: the client
        //   got bytes, so closing it as `billed` with no debit would turn the
        //   delivery free forever. Reconciliation bills it from the durable
        //   usage row if one landed, or from the delivered-output checkpoint.
        if posted
            && (usage_backed || !delivered)
            && let Some(reservation_id) = lane.reservation_id()
            && let Err(error) = reservation::close_after_settle(
                Arc::clone(&self.conn),
                reservation_id,
                key,
                // A billed client cancel closes the intent as billed; a cancel
                // without a usage row uses the delivered-output checkpoint.
                delivered || (client_cancelled && usage_backed),
            )
            .await
        {
            tracing::warn!(
                %error,
                api_key_id = key,
                "credit settlement: durable intent left open for reconciliation"
            );
        }

        tracing::debug!(
            api_key_id = key,
            posted,
            "credit settlement: releasing key lane"
        );
        drop(lane);
    }

    /// Mark the usage row billable and apply its ledger debit. Idempotent:
    /// re-marking is a no-op and the debit is guarded by `credits_debited`.
    async fn post_debit(&self, debit: &PendingDebit) -> openproxy_types::Result<()> {
        let repo = self.repo();
        let request_id = debit.request_id.clone();
        let attempt = debit.attempt;
        let target_id = debit.target_id;
        match tokio::task::spawn_blocking(move || {
            repo.mark_winner_usage_row(&request_id, attempt, target_id)
        })
        .await
        {
            Ok(result) => result,
            Err(error) => Err(CoreError::Internal(format!(
                "credit debit post failed to run: {error}"
            ))),
        }
    }

    /// Drain the durable usage journal so pending debits are applied to the
    /// ledger before anyone reads the balance.
    ///
    /// Runs on the blocking pool: the drain takes the single writer connection,
    /// which must never be held across an `await`. The error is propagated —
    /// callers fail closed on it.
    async fn drain_usage_journal(&self) -> openproxy_types::Result<()> {
        let conn = Arc::clone(&self.conn);
        match tokio::task::spawn_blocking(move || crate::worker::replay_pending(&conn)).await {
            Ok(result) => result,
            Err(error) => Err(CoreError::Internal(format!(
                "credit journal drain failed to run: {error}"
            ))),
        }
    }
}

/// The one prepaid-credit admission policy, shared by chat (`Pipeline::run`
/// through `Pipeline::admit_credit_request`) and the unary executors.
///
/// Order matters and is the whole invariant: acquire the key lane first, then
/// clear anything the key still owes, then apply durable debits, and only then
/// read the balance. Every failure on that path is an error, never a fallback
/// to "spend anyway": a balance that cannot be recomputed is a balance that
/// cannot admit a request.
async fn admit_prepaid<D: CreditGateDb>(
    db: &D,
    api_key_id: Option<ApiKeyId>,
    request_id: String,
    attempt: u8,
    budget_units: Option<i64>,
) -> openproxy_types::Result<Option<CreditsLaneGuard>> {
    let Some(key_id) = api_key_id else {
        return Ok(None);
    };
    let key = key_id.0;
    let db_id = db.db_id();
    // Lanes and markers are process-wide statics, so this handle is the same
    // gate a `Pipeline` holds.
    let gate = CreditsAdmissionGate::new();

    // 1. Cheap probe: keys without ledger rows are free/unlimited and never
    //    serialized.
    let credit_based = match db.credit_based(key).await {
        Ok(governed) => governed,
        Err(error) => {
            // A failing probe means the balance read would fail too. Gate the
            // key anyway so the failure surfaces as one explicit error instead
            // of silently skipping serialization.
            tracing::warn!(
                %error,
                api_key_id = key,
                "credit admission: ledger probe failed; treating key as credit-governed"
            );
            true
        }
    };
    if !credit_based {
        return Ok(None);
    }

    // 2. One request per key from here until its debit is persisted: the
    //    in-process lane, then the cross-process lock next to the database
    //    file. Both are held across every step below.
    let db_path = db.database_path().await?;
    let mut lane = gate.acquire_leased(db_id, &db_path, key).await?;

    // 3. Charges this key still owes from a previous request.
    if let Some(debit) = gate.pending_debit(db_id, key) {
        match db.post_chat_debit(&debit).await {
            Ok(()) => gate.clear_pending_debit(db_id, key),
            Err(error) => {
                tracing::error!(
                    %error,
                    api_key_id = key,
                    "credit admission: previous debit still unposted; denying request"
                );
                return Err(CoreError::Internal(
                    "credit ledger: a previous charge for this key is still unposted".into(),
                ));
            }
        }
    }
    if let Some(pending) = gate.pending_unary_usage(db_id, key) {
        match db.post_unary_usage(&pending).await {
            // Applied now, or durably journalled: either way the marker's job
            // is done and a later drain owns the rest.
            Ok(()) | Err(UnaryPersistError::Journaled(_)) => {
                gate.clear_pending_unary_usage(db_id, key);
            }
            Err(UnaryPersistError::Lost(error)) => {
                tracing::error!(
                    %error,
                    api_key_id = key,
                    "credit admission: previous unary usage still unposted; denying request"
                );
                return Err(CoreError::Internal(
                    "credit ledger: a previous usage row for this key is still unposted".into(),
                ));
            }
        }
    }

    // 4. Durable debits whose settlement task never ran (aborted request,
    //    closed worker channel). Without them the balance below is stale.
    if let Err(error) = db.drain_journal().await {
        tracing::error!(
            %error,
            api_key_id = key,
            "credit admission: journal drain failed; denying request instead of trusting a stale balance"
        );
        return Err(CoreError::Internal(
            "credit ledger: pending charges could not be applied before admitting".into(),
        ));
    }

    // 4b. Durable intents of this key whose owner never closed them: a crashed,
    //     killed or abandoned request. Holding the cross-process lock proves no
    //     live owner exists for any of them, so they are resolved here from the
    //     durable usage table — after the drain, before the balance is read.
    if let Err(error) = reservation::reconcile(db.writer_arc(), key).await {
        tracing::error!(
            %error,
            api_key_id = key,
            "credit admission: durable intent reconciliation failed; denying request"
        );
        return Err(CoreError::Internal(
            "credit ledger: request intents for this key could not be resolved".into(),
        ));
    }

    // 5. Only now is the balance authoritative.
    let balance = match db.balance(key).await {
        Ok(balance) => balance,
        Err(error) => {
            tracing::error!(
                %error,
                api_key_id = key,
                "credit admission: balance read failed; denying request"
            );
            return Err(CoreError::Internal("credit balance check failed".into()));
        }
    };
    if balance <= 0 {
        tracing::warn!(
            api_key_id = key,
            "credit admission: balance exhausted while request was queued for its key; denying"
        );
        return Err(CoreError::InsufficientCredits { balance_usd: 0.0 });
    }

    // 6. The request's budget: the caller's explicit quote when it supplied one,
    //    never more than the balance that is actually available. It travels in
    //    the guard (`budget_units`) so the dispatcher can refuse a response that
    //    would cost more than what this request reserved.
    let quoted = budget_units.filter(|units| *units > 0);
    let budget = quoted.map_or(balance, |units| units.min(balance));

    // 7. Durable intent BEFORE anything is dispatched. From here a crash leaves
    //    evidence that this request existed, and the next admission of the key
    //    (in this or another process) resolves it from real usage instead of
    //    trusting a balance that misses this request.
    let reservation_id = match reservation::hold(
        db.writer_arc(),
        key,
        request_id,
        attempt,
        budget,
        quoted.is_some(),
    )
    .await
    {
        Ok(id) => id,
        Err(error) => {
            tracing::error!(
                %error,
                api_key_id = key,
                "credit admission: request intent could not be recorded; denying request"
            );
            return Err(CoreError::Internal(
                "credit ledger: the request intent could not be recorded before dispatch".into(),
            ));
        }
    };
    lane.set_budget_units(budget);
    lane.set_reservation(reservation_id);
    Ok(Some(lane))
}

mod unary;
pub use unary::{admit_unary_credit, admit_unary_credit_with_request, settle_unary_credit};

// These tests seed credit tables directly: they only exist with payments.
#[cfg(all(test, feature = "payments"))]
#[path = "credits_gate_tests.rs"]
mod tests;
