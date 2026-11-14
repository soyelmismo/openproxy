//! Prepaid-credit admission for the non-chat executors.
//!
//! Audio, embeddings, images and SystemOne never go through `Pipeline::run`, so
//! they recorded usage (and therefore applied the ledger debit) *after* the
//! fact with no admission step: N concurrent requests for one prepaid key all
//! read the same balance, all dispatched, and every debit after the first was
//! clamped to whatever was left. This lease closes that gap by taking the same
//! process-wide per-key lane chat uses, held from before the upstream call
//! until the usage row — the row whose insert applies the debit — is persisted.
//!
//! It also closes the *single-request* half of the gap: before dispatch the
//! executor asks [`Self::authorize_dispatch`] to price the request from the
//! client's own input ([`super::budget`]). A request whose maximum cost does
//! not fit the budget the gate admitted it against is refused while nothing has
//! been sent upstream, and its settlement fills the quantities the upstream
//! omitted from that same bound, so a missing `usage` block bills at the bound
//! instead of at zero.
//!
//! What this does **not** promise:
//!
//! - Only keys with ledger rows are serialized or priced; free/unlimited keys
//!   keep full concurrency and never depend on pricing.
//! - Cross-process mutual exclusion is the gate's file lease
//!   ([`CreditsLaneGuard`] owns it); the guarantee here is that this request
//!   holds *that* key's turn from admission to the posted debit.
//! - An upstream that reports more than the admitted bound is not silently
//!   clamped: the row keeps what it reported (the overrun is evidence) and the
//!   ledger's own debit clamp is what protects the balance.

use openproxy_db::DbPool;
use openproxy_pipeline::credits_gate::{CreditsLaneGuard, admit_unary_credit, settle_unary_credit};
use openproxy_types::CoreError;
use openproxy_types::Result;
use openproxy_types::ids::ApiKeyId;

use super::UnaryTarget;
use super::budget::{self, BudgetQuote, HostLocal, UnaryCharge};
use super::usage::{UnaryUsageArgs, unary_usage_input};

/// One unary request's exclusive turn on its prepaid key.
///
/// Dropping it without [`Self::settle`] releases the lane and bills nothing,
/// which is the right outcome for a request that never produced a response
/// (upstream error, cancellation, no healthy target).
pub struct UnaryCreditLease {
    lane: Option<CreditsLaneGuard>,
}

impl UnaryCreditLease {
    /// Take the key's turn. A key without ledger rows gets a no-op lease and is
    /// never serialized.
    pub async fn admit(db_pool: &DbPool, api_key_id: Option<ApiKeyId>) -> Result<Self> {
        Ok(Self {
            lane: admit_unary_credit(db_pool, api_key_id).await?,
        })
    }

    /// Whether this request spends a prepaid balance, i.e. whether pricing and
    /// quantity bounds apply at all. Free/unlimited keys answer `false`.
    #[must_use]
    pub fn is_prepaid(&self) -> bool {
        self.lane.is_some()
    }

    /// Remaining balance in micro-USDC, read while this request holds the key
    /// lane.
    ///
    /// Nothing else in this process can spend this key between admission and
    /// this read (the lane is held) and admission already drained every owed
    /// debit, so the value is the same one the gate admitted against.
    ///
    /// # Errors
    /// Propagates ledger failures: a balance that cannot be read cannot admit
    /// anything.
    pub async fn remaining_units(&self, db_pool: &DbPool) -> Result<Option<i64>> {
        let Some(lane) = self.lane.as_ref() else {
            return Ok(None);
        };
        let key = lane.key_id();
        db_pool
            .spawn_read(move |conn| openproxy_db::credits_ledger::balance_units(conn, key))
            .await
            .map(Some)
    }

    /// Price one request before it is dispatched and refuse it when its maximum
    /// cost does not fit the budget the gate admitted it against.
    ///
    /// The budget is [`CreditsLaneGuard::budget_units`], captured by admission
    /// from the authoritative balance read taken under the key's lock — not a
    /// fresh read, which could only differ by a debit this lane already owns.
    ///
    /// Returns `None` for a free/unlimited key — callers pass that through to
    /// [`Self::settle_quoted`], which then behaves exactly like
    /// [`Self::settle`]. A prepaid key gets the bound every later settlement is
    /// clamped to.
    ///
    /// `local` carries proof that the request runs on this host (the native
    /// `laya` / `embeddinggemma` engines): see [`super::budget::HostLocal`].
    ///
    /// # Errors
    /// - [`CoreError::InsufficientCredits`] when the balance is exhausted (0):
    ///   a positive balance is always admitted (last-draw; the debit clamps).
    /// - [`CoreError::ServiceUnavailable`] when the request has no usable
    ///   price (fail closed: it must not dispatch unbilled).
    /// - [`CoreError::Validation`] when no billable quantity can be derived.
    pub async fn authorize_dispatch(
        &self,
        db_pool: &DbPool,
        target: &UnaryTarget,
        charge: UnaryCharge<'_>,
        local: Option<HostLocal<'_>>,
    ) -> Result<Option<BudgetQuote>> {
        let Some(lane) = self.lane.as_ref() else {
            return Ok(None);
        };
        let quote = budget::quote(db_pool, target, charge, local).await?;
        let budget = lane.budget_units();
        if budget <= 0 {
            tracing::warn!(
                api_key_id = lane.key_id(),
                provider = %target.provider,
                model = %target.upstream_model,
                "prepaid balance exhausted; refusing before dispatch"
            );
            return Err(CoreError::InsufficientCredits { balance_usd: 0.0 });
        }
        if quote.units() > budget {
            // LAST-DRAW: the real cost may still fit the remainder; the debit
            // clamps to whatever is left, so the plan closes at exactly 0 and
            // the next request is refused above.
            tracing::warn!(
                api_key_id = lane.key_id(),
                required_units = quote.units(),
                budget_units = budget,
                provider = %target.provider,
                model = %target.upstream_model,
                "credit last-draw: admitted a request whose maximum exceeds the balance"
            );
        }
        Ok(Some(quote))
    }

    /// Persist the usage row — which applies the debit — and only then release
    /// the lane.
    ///
    /// The error is returned, not logged and dropped: a caller that cannot
    /// prove the charge was recorded must not answer as if it had been. The
    /// gate keeps the unposted row for the key, so the following admission
    /// retries it and refuses to spend a balance that misses this debit.
    pub async fn settle(&mut self, db_pool: &DbPool, args: &UnaryUsageArgs<'_>) -> Result<()> {
        let lane = self.lane.take();
        settle_unary_credit(db_pool, lane, unary_usage_input(args)).await
    }

    /// Settle a request that was priced by [`Self::authorize_dispatch`], filling
    /// the quantities the upstream omitted from what admission authorized.
    ///
    /// Without a quote (free key) this is exactly [`Self::settle`]; with one, a
    /// missing (or zero) upstream count is replaced by the bound, so an
    /// upstream that reports nothing can never bill zero. A count *above* the
    /// bound is kept as reported — that is a real overrun and the row is the
    /// evidence for it — see [`BudgetQuote::apply_quote`].
    pub async fn settle_quoted(
        &mut self,
        db_pool: &DbPool,
        args: &UnaryUsageArgs<'_>,
        quote: Option<&BudgetQuote>,
    ) -> Result<()> {
        match quote {
            None => self.settle(db_pool, args).await,
            Some(quote) => {
                let mut quoted = args.clone();
                quote.apply_quote(&mut quoted);
                self.settle(db_pool, &quoted).await
            }
        }
    }
}

#[cfg(all(test, feature = "payments"))]
#[path = "credit_tests.rs"]
mod tests;
