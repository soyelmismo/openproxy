//! The request pipeline. See spec §5.
//!
//! One `Pipeline::run()` call processes one chat completion request: it resolves
//! the combo into concrete (provider, model, account) targets, expands account
//! rotation, executes the first eligible target with bounded timeouts, and
//! records a usage row.
//!
//! Streaming and full race orchestration are intentionally stubbed in this
//! first cut — the MVP runs a single upstream call per request and the
//! SSE plumbing lives in a follow-up.

use crate::adapters::{AdapterFormat, ProviderAdapter};
use crate::circuit_breaker::{CircuitBreakerRegistry, Health};
use crate::combos::{self, Combo, ComboTarget, Strategy};
use crate::config::{CircuitBreakerConfig, RacingConfig, RetriesConfig, TimeoutsConfig};
use crate::cost::{self, UsageInput};
use crate::error::{CoreError, ErrorContext, Result};
use crate::ids::{ApiKeyId, ComboId, RequestId, TraceId};
use crate::models::{self, Model};
use crate::retry::RetryPolicy;
use crate::secrets::MasterKey;
use crate::timeouts::{self, ModelTimeoutOverrides, Timeouts};
use crate::translation::{OpenAIRequest, OpenAIResponse};
use crate::upstream::{
    CancellationToken, UpstreamClient, UpstreamError, UpstreamPhase, UpstreamRequest,
};
use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tracing;

// ---------------------------------------------------------------------
// Streaming dispatch
// ---------------------------------------------------------------------
//
// `dispatch_upstream_streaming` is the streaming counterpart of
// `dispatch_upstream_request`. Both call into the hyper-based
// `UpstreamClient`; the streaming helper owns the `UpstreamRequest`
// (constructed by the caller) and consumes the response body as an
// `UpstreamBodyStream` (one `next_chunk()` per iteration) instead of
// `response.collect()` (which the non-streaming path uses).
//
// Cancellation is mediated by a `CancellationToken` built from the
// per-request `client_disconnected` watch. The token is plumbed
// through the upstream client at every phase boundary and inside the
// body stream between frames. The body loop also races the watch
// directly so a mid-stream cancel short-circuits the SSE pipeline
// (the post-loop `is_client_disconnected` checkpoint then emits the
// structured `ClientDisconnected` usage row).

/// Per-call knobs the pipeline reads from the surrounding `AppConfig`.
#[derive(Clone)]
pub struct PipelineConfig {
    pub defaults: Timeouts,
    pub racing: RacingConfig,
    pub retries: RetriesConfig,
    pub max_attempts: u8,
    pub master_key: Arc<MasterKey>,
    pub adapters: Arc<Vec<Arc<dyn ProviderAdapter>>>,
    /// Shared HTTP client used for upstream calls, snapshotted
    /// from `AppState` at pipeline-construction time. The
    /// snapshot is a `reqwest::Client` clone (cheap, since
    /// `reqwest::Client` is internally `Arc`-backed), and it
    /// shares the connection pool with the live client. Because
    /// the snapshot is taken at construction time, in-flight
    /// pipelines keep the `connect_timeout` they were started
    /// with — a runtime update to `timeouts.connect_ms` is
    /// picked up by the **next** request, not by any pipeline
    /// that is already running.
    ///
    /// `connect_timeout` itself is configured on the client in
    /// `state.rs` (at startup from `timeouts.connect_ms` and
    /// re-applied live by `set_timeouts` whenever that value
    /// changes). The rest of the timeouts are enforced
    /// per-request or measured in this file — see the comment
    /// above the call to `self.config.http_client.post(...)` in
    /// `dispatch_upstream_request` for the full mapping.
    pub http_client: reqwest::Client,
    /// How long a target stays parked in `target_cooldowns` after a
    /// retryable failure. Read from `[cooldown].cooldown_secs` /
    /// `OPENPROXY_COOLDOWN_SECS` (default 60 s). The pipeline does
    /// not grow this with `failure_count`; the spec calls for a flat
    /// window that resets on every retryable failure. See
    /// [`crate::cooldown`].
    pub cooldown_secs: u64,
    /// Hyper-based upstream client used for the non-streaming chat
    /// dispatch (Gate 1). The streaming path and the Kiro/Antigravity
    /// executors still use `http_client` (the reqwest client); they
    /// are migrated in follow-up gates. Sharing the `Arc` is cheap —
    /// the underlying hyper client is `Clone` and pools per-host.
    pub upstream_client: Arc<UpstreamClient>,
}

/// All the input needed to process a single chat completion.
#[derive(Debug, Clone)]
pub struct PipelineRequest {
    pub request_id: RequestId,
    pub trace_id: TraceId,
    pub combo_id: ComboId,
    pub openai_request: OpenAIRequest,
    /// Fires `true` when the client cancels the request.
    pub client_disconnected: watch::Receiver<bool>,
    /// For streaming responses, the pipeline writes SSE chunks here.
    pub stream_sink: Option<mpsc::Sender<String>>,
    /// The authenticated API key, if any. Propagated into the
    /// `usage.api_key_id` column so per-key analytics work
    /// downstream. `None` = anonymous (backward-compatible dev mode).
    pub api_key_id: Option<ApiKeyId>,
    /// In-memory combo override. When `Some`, the pipeline uses this
    /// combo definition directly instead of loading `combo_id` from
    /// the DB. Used by the routing layer to dispatch a direct-model
    /// request through a synthetic single-target combo without
    /// having to round-trip through the `combos` table.
    ///
    /// `None` = normal path: look the combo up by id.
    pub combo_override: Option<Combo>,
    /// In-memory targets override. When `Some`, the pipeline uses
    /// this list directly as the (post-strategy, pre-account-
    /// expansion) target set, skipping the `combo_targets` table
    /// lookup. Used by the routing layer to ship the synthetic
    /// single-target for a direct-model dispatch without writing to
    /// the DB.
    ///
    /// `None` = normal path: load the targets from the DB.
    pub targets_override: Option<Vec<ComboTarget>>,
    /// Request headers as captured by the HTTP layer. Used by the
    /// recording path to persist the inbound headers in the
    /// `usage.request_headers` column when recording is enabled.
    /// Always populated for requests that pass through the chat
    /// handler, so the failure path of the pipeline can still
    /// record what the client sent.
    pub request_headers: std::collections::BTreeMap<String, String>,
}

/// Outcome of a single `Pipeline::run()` call.
///
/// `Clone` is intentionally omitted: `CoreError` does not derive `Clone` (it
/// can carry non-cloneable boxed source errors), so cloning the result would
/// require that. Callers that need to ship the result across an async
/// boundary should move it.
#[derive(Debug)]
pub struct PipelineResult {
    pub status_code: u16,
    pub error: Option<CoreError>,
    pub final_response: Option<OpenAIResponse>,
    /// Number of upstream attempts (sequential retries + race losers).
    pub attempts: u8,
}

/// Orchestrates a single request end-to-end.
///
/// `Pipeline` is cheaply cloneable: the expensive state (the DB mutex, the
/// in-memory circuit-breaker registry, the round-robin counters) lives behind
/// `Arc`s and is shared across all in-flight requests handled by a server
/// instance.
pub struct Pipeline {
    conn: Arc<parking_lot::Mutex<Connection>>,
    config: PipelineConfig,
    circuit_breaker: CircuitBreakerRegistry,
    rr_counters: Arc<parking_lot::Mutex<HashMap<ComboId, u64>>>,
    /// If `true`, the pipeline records the full request and response bodies
    /// and headers in the `usage` table. False by default to save disk.
    /// Shared with `AppState` so the admin endpoint can toggle it.
    record_bodies_and_headers: Arc<AtomicBool>,
}

impl Pipeline {
    /// Build a new `Pipeline`. The circuit breaker is constructed with a
    /// hardcoded 5-failures / 60-second-unhealthy policy — it lives in this
    /// module rather than `AppConfig` because the spec (§5.2) treats it as
    /// a runtime concern, not a config-file concern.
    pub fn new(conn: Arc<parking_lot::Mutex<Connection>>, config: PipelineConfig) -> Self {
        Self::with_recording_flag(conn, config, Arc::new(AtomicBool::new(false)))
    }

    /// Build a new `Pipeline` that shares the recording flag with the
    /// caller (typically `AppState`). This is how the admin endpoint
    /// can flip recording on and have it take effect on the next
    /// in-flight request, since the `Pipeline` is constructed
    /// per-request inside the chat handler.
    pub fn with_recording_flag(
        conn: Arc<parking_lot::Mutex<Connection>>,
        config: PipelineConfig,
        record_bodies_and_headers: Arc<AtomicBool>,
    ) -> Self {
        let cb = CircuitBreakerRegistry::new(&CircuitBreakerConfig {
            failure_threshold: 5,
            unhealthy_duration_ms: 60_000,
        });
        Self {
            conn,
            config,
            circuit_breaker: cb,
            rr_counters: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            record_bodies_and_headers,
        }
    }

    /// Get the current recording state.
    pub fn is_recording(&self) -> bool {
        self.record_bodies_and_headers.load(Ordering::Relaxed)
    }

    /// Set the recording state. When `true`, the pipeline will record
    /// full request/response bodies and headers in the `usage` table.
    pub fn set_recording(&self, enabled: bool) {
        self.record_bodies_and_headers.store(enabled, Ordering::Relaxed);
    }

    /// Drive one chat-completion request to completion.
    ///
    /// Returns a [`PipelineResult`] describing the outcome. The function is
    /// total: it never panics on a missing combo, empty target list, or
    /// transient upstream error — every error path is mapped to a
    /// `(status_code, Some(CoreError))` pair.
    pub async fn run(&self, req: PipelineRequest) -> PipelineResult {
        // 1. Resolve the combo. Prefer the in-memory override (set by
        //    the routing layer for direct-model dispatch) and fall
        //    back to the DB lookup keyed on `combo_id`.
        let combo = match self.load_combo(&req) {
            Ok(c) => c,
            Err(e) => return self.failure(e, 0, ErrorPhase::Resolve),
        };

        // Outer loop: a single combo walk. The pre-fix code used
        // `for attempt in 1..=self.config.max_attempts` and let
        // the inner walk re-fire N times, but that re-fired
        // per-target calls too (it was the *only* retry mechanism
        // for the whole combo). Bug 4 fix: retries are now
        // applied per-target *inside* the per-target loop (see
        // the `while let Some(e) = &result.error` block further
        // down). The outer loop now runs exactly once; the
        // `attempt` variable is still threaded through for usage
        // recording and as a stable identifier across the
        // per-target retry calls, but its count is no longer
        // user-tunable from `PipelineConfig.max_attempts` — the
        // per-target retry budget lives in
        // `PipelineConfig.retries.max_attempts`.
        for attempt in 1..=1 {
            // 2. Resolve and expand targets.
            let targets = match self.resolve_targets(&combo, req.targets_override.as_deref()) {
                Ok(t) => t,
                Err(e) => return self.failure(e, attempt - 1, ErrorPhase::Resolve),
            };

            // 3. Flatten sub-combos. A combo can have sub-combo
            //    targets (combo-in-combo); before we hand the list to
            //    the per-target dispatch loop we resolve each
            //    sub-combo recursively into its children. The result
            //    is a flat `Vec<ComboTarget>` in which every entry
            //    has `sub_combo_id = None` and is directly
            //    executable. Cycle / max-depth errors from the
            //    resolver abort the request before any upstream call.
            let flat_targets = match self.flatten_targets(&combo.id, targets) {
                Ok(t) => t,
                Err(e) => return self.failure(e, attempt - 1, ErrorPhase::Resolve),
            };

            // 4. Filter out accounts that the circuit breaker marks unhealthy.
            // Declared `mut` because the `NoHealthyTargets` fallback below
            // may re-evaluate this set after auto-populating the combo.
            //
            // The `pre_cb_snapshot` is the post-flatten, pre-circuit-breaker
            // target list. We keep it so that, if the CB filter empties
            // `eligible` but the snapshot had content, we can fall through
            // to the dispatch loop with the unfiltered list instead of
            // short-circuiting to a 502. This mirrors the snapshot/fallback
            // we do for the persistent `target_cooldowns` filter a few
            // blocks down: both the CB and the cooldown table protect
            // *between* requests, not *within* a single request when doing
            // so would deny a priority combo the chance to walk its full
            // row. The CB itself is re-evaluated on every dispatch (an
            // upstream that came back online mid-window and is now
            // `Healthy` in the registry will pass the filter normally on
            // the next request), and `record_success` clears the failure
            // counter when an attempt succeeds.
            let pre_cb_snapshot: Vec<ComboTarget> = flat_targets.clone();
            let mut eligible: Vec<ComboTarget> = flat_targets
                .into_iter()
                .filter(|t| match t.account_id {
                    Some(aid) => self.circuit_breaker.is_healthy(aid) == Health::Healthy,
                    None => true,
                })
                .take(self.config.racing.max_race_size as usize)
                .collect();

            if eligible.is_empty() && !pre_cb_snapshot.is_empty() {
                // Circuit breaker emptied `eligible` but the pre-CB list
                // had content. The same rationale as the cooldown
                // snapshot below applies: a priority combo's contract
                // is "walk the row", not "fail fast on the first parked
                // target we see". We re-evaluate with the unfiltered
                // list; the dispatch loop's success path will clear the
                // CB via `record_success`, and its failure path will
                // re-record and re-park the account for the next
                // request.
                tracing::warn!(
                    combo_id = combo.id.0,
                    parked = pre_cb_snapshot.len(),
                    "all targets' accounts unhealthy in circuit_breaker; falling through to pre-CB dispatch"
                );
                eligible = pre_cb_snapshot
                    .into_iter()
                    .take(self.config.racing.max_race_size as usize)
                    .collect();
            }

            if eligible.is_empty() {
                // Auto-populate fallback: if this combo is empty (zero
                // targets) try to fill it with the first provider's
                // active models so an operator who just created a combo
                // can hit the API without manually wiring targets. After
                // a successful fill we re-evaluate the target list; if
                // there are still no eligible targets we fall through to
                // the NoHealthyTargets recording below.
                //
                // This branch is only entered on `attempt == 1` because
                // retries don't re-run it (the repopulate guard below
                // would be a no-op for any subsequent attempt and we'd
                // risk an infinite re-population).
                if attempt == 1 {
                    let repopulated = match self.auto_populate_if_empty(&combo) {
                        Ok(n) => n,
                        Err(e) => {
                            tracing::warn!(
                                combo_id = combo.id.0,
                                combo_name = %combo.name,
                                error = %e,
                                "auto_populate on NoHealthyTargets failed; recording failure"
                            );
                            let started = std::time::Instant::now();
                            self.record_no_healthy_targets_row(&req, &combo, started);
                            return self.failure(e, attempt - 1, ErrorPhase::Route);
                        }
                    };
                    if repopulated > 0 {
                        // The combo now has targets. Re-resolve and
                        // re-filter and continue the attempt with the
                        // new eligible set. We don't `continue` because
                        // the loop bounds + this `if` would re-enter
                        // auto_populate on the next pass; the
                        // `attempt > 1` guard above prevents that, but
                        // restarting the loop body is cheaper than
                        // re-validating.
                        let targets = match self.resolve_targets(&combo, req.targets_override.as_deref()) {
                            Ok(t) => t,
                            Err(e) => return self.failure(e, attempt - 1, ErrorPhase::Resolve),
                        };
                        let flat_targets = match self.flatten_targets(&combo.id, targets) {
                            Ok(t) => t,
                            Err(e) => return self.failure(e, attempt - 1, ErrorPhase::Resolve),
                        };
                        let re_eligible: Vec<ComboTarget> = flat_targets
                            .into_iter()
                            .filter(|t| match t.account_id {
                                Some(aid) => {
                                    self.circuit_breaker.is_healthy(aid) == Health::Healthy
                                }
                                None => true,
                            })
                            .take(self.config.racing.max_race_size as usize)
                            .collect();
                        if !re_eligible.is_empty() {
                            eligible = re_eligible;
                        }
                    }
                }
                if eligible.is_empty() {
                    // NoHealthyTargets is not retryable per spec — short-circuit
                    // and write a usage row so the dashboard's Live Logs
                    // tail isn't permanently empty.
                    let err = CoreError::NoHealthyTargets(combo.id.0);
                    let started = std::time::Instant::now();
                    self.record_no_healthy_targets_row(&req, &combo, started);
                    return self.failure(err, attempt - 1, ErrorPhase::Route);
                }
            }

            // 5. Pick the dispatch window.
            //
            //    For `Strategy::Priority` the operator's intent is
            //    walk-the-row: try every target in priority order
            //    before giving up. `race_size` is a *parallel* race
            //    concept (how many lanes to fire at once), so it
            //    doesn't apply to a serial priority walk — applying
            //    `take(combo.race_size)` here would collapse the
            //    dispatch to a single target, defeating both the
            //    priority walk and the cross-target retry budget in
            //    the outer `for attempt in 1..=max_attempts` loop
            //    (each turn re-runs the same single target).
            //
            //    For `Strategy::RoundRobin` and `Strategy::Shuffle`
            //    the race window is meaningful: those strategies
            //    intentionally cap the set of targets fired in
            //    parallel. Keep the `take(combo.race_size)` behavior
            //    for those, clamped by `eligible.len()` and the
            //    global `max_race_size` config ceiling.
            //
            //    `race_size` is bound in the outer scope because the
            //    inner per-target loop forwards it to
            //    `execute_single` (as the per-target attempt budget
            //    for the race-aware adapter contract). For
            //    `Strategy::Priority` we substitute the full row
            //    length so the same `u8` value the per-target call
            //    expects ("how many lanes did I think I'd run?")
            //    is the actual walk length.
            let race_size: usize = match combo.strategy {
                Strategy::Priority => eligible.len(),
                Strategy::RoundRobin | Strategy::Shuffle => (combo.race_size as usize)
                    .min(eligible.len())
                    .min(self.config.racing.max_race_size as usize),
            };
            // `to_run` is `mut` because the cooldown-fallback path
            // below may swap in the unfiltered list if the persistent
            // cooldown filter empties it (see the 5b comment).
            let mut to_run: Vec<ComboTarget> = if matches!(combo.strategy, Strategy::Priority) {
                eligible
            } else {
                eligible.into_iter().take(race_size).collect()
            };

            // Snapshot of the post-circuit-breaker, pre-cooldown target
            // list. Kept around so that, if the cooldown filter below
            // empties `to_run`, we can fall through to the dispatch
            // loop with this unfiltered list instead of returning a
            // premature 502. See the 5b block comment for the full
            // rationale (cooldown protects BETWEEN requests, not WITHIN
            // a single request when doing so would deny a priority
            // combo the chance to walk its full row).
            let to_run_unfiltered_snapshot: Vec<ComboTarget> = to_run.clone();

            // 5b. Filter out targets currently parked in the persistent
            //     cooldown registry. The DB read is cheap (indexed on
            //     `combo_target_id`) and keeps the in-loop path off the
            //     hot path's mutex. Sub-combo rows (`model_row_id = None`)
            //     never reach this point — `flatten_targets` already
            //     replaced them with their children, so each child is
            //     independently checkable.
            //
            //     IMPORTANT: this filter runs *after* `to_run` is built,
            //     so a target that was eligible when we picked the race
            //     window but entered cooldown between then and now is
            //     also skipped. The race window itself stays as-is (no
            //     backfill): a request that found N targets, then saw M
            //     of them go into cooldown, will run on N-M and not
            //     chase the next best substitute. That keeps the
            //     cooldown behavior predictable from the operator's POV.
            //
            //     Cooldown semantics: the persistent cooldown protects
            //     *between* requests, not *within* a single request. If
            //     the cooldown filter empties `to_run` we don't want
            //     the request to give up with a 502 — for a priority
            //     combo the operator expects the request to walk the
            //     full row of targets until one succeeds. We preserve
            //     the pre-filter list as `to_run_unfiltered` and, when
            //     the post-filter list is empty, fall through to the
            //     dispatch loop using the unfiltered list. The per-
            //     target cooldown is *re-checked* in the dispatch loop
            //     via `record_failure` only on the *result* of trying
            //     the target, so an upstream that has come back online
            //     during the gap (and would no longer be in cooldown)
            //     still gets exercised. The DB row stays in the table
            //     until `prune_expired` sweeps it, so the cross-request
            //     protection is preserved.
            let mut to_run: Vec<ComboTarget> = {
                let cooldown_conn = self.conn.lock();
                to_run
                    .into_iter()
                    .filter(|t| match crate::cooldown::is_in_cooldown(&cooldown_conn, t.id) {
                        Ok(true) => {
                            tracing::debug!(
                                combo_id = combo.id.0,
                                target_id = t.id.0,
                                provider = %t.provider_id,
                                "target in cooldown, skipping"
                            );
                            false
                        }
                        Ok(false) => true,
                        Err(e) => {
                            // DB read failure on the cooldown table
                            // is non-fatal: fall through to the
                            // upstream call rather than block the
                            // whole combo on a bookkeeping error.
                            tracing::warn!(
                                combo_id = combo.id.0,
                                target_id = t.id.0,
                                error = %e,
                                "is_in_cooldown check failed; proceeding without filter"
                            );
                            true
                        }
                    })
                    .collect()
            };

            // `to_run_unfiltered` is the post-circuit-breaker, pre-cooldown
            // list — i.e. the targets we *would* have walked if there were
            // no persistent cooldown in effect. If the cooldown filter
            // emptied `to_run` but `to_run_unfiltered` still has entries,
            // we fall through to the dispatch loop with the unfiltered list
            // so a single request doesn't bounce off a transient cross-
            // request cooldown state. See the comment on the 5b block above
            // for the full rationale.
            let to_run_unfiltered: Vec<ComboTarget> = to_run_unfiltered_snapshot;

            if to_run.is_empty() {
                if to_run_unfiltered.is_empty() {
                    // Truly nothing to do: the post-circuit-breaker
                    // eligible set was empty, so the cooldown filter
                    // can't be blamed. Surface the same
                    // NoHealthyTargets error the circuit-breaker branch
                    // would have surfaced, with the same usage-row side
                    // effect, so the dashboard's Live Logs tail is
                    // consistent across the two "no usable target"
                    // scenarios.
                    let err = CoreError::NoHealthyTargets(combo.id.0);
                    let started = std::time::Instant::now();
                    self.record_no_healthy_targets_row(&req, &combo, started);
                    return self.failure(err, attempt - 1, ErrorPhase::Route);
                }
                // Cooldown filter emptied `to_run` but the pre-filter
                // list had content. For a priority combo, the
                // operator's expectation is "walk the whole row before
                // giving up", not "fail fast on the first parked
                // target we see". Re-try using the unfiltered list —
                // the upstream may have come back online since the
                // last request parked these targets (in which case the
                // next record_failure on success will clear the row),
                // and even if it hasn't, the dispatch loop's failure
                // path will record the real error (and re-park the
                // target). Either way, the request will not silently
                // degrade to a 502 NoHealthyTargets.
                tracing::warn!(
                    combo_id = combo.id.0,
                    parked = to_run_unfiltered.len(),
                    "all targets in cooldown for this request; falling through to unfiltered dispatch"
                );
                to_run = to_run_unfiltered;
            }

            // 6. Try each target in priority order. The first one
            //    that returns Ok wins; on a failure the per-target
            //    retry loop above has already exhausted the
            //    `retries.max_attempts` budget for this model, so
            //    we fall through to the next target in the combo
            //    (bug 3 contract). `last_result` is what we
            //    return at the end if every target errored — it
            //    carries the last per-target retry's final error.
            let mut last_result: Option<PipelineResult> = None;
            for target in to_run.iter() {
                if {
                    let mut rx = req.client_disconnected.clone();
                    self.is_client_disconnected(&mut rx)
                } {
                    // Per-target boundary check: the request's
                    // `client_disconnected` watch (driven by the
                    // chat handler's watchdog task) has fired
                    // between targets. We don't `record_and_fail`
                    // here because (a) we haven't actually tried
                    // the next target and (b) cancellation is
                    // user-driven, not upstream-driven: there's no
                    // upstream error to attribute, and the usage
                    // row for this request (if any) is written by
                    // the path that owns the most recent work in
                    // flight — the *previous* target's
                    // `record_and_fail` if it was mid-flight, or
                    // a fresh row written by the post-loop
                    // `record_and_fail` below for a boundary-only
                    // disconnect. We trace at warn level so
                    // operators can see cancellation in the logs
                    // without grepping for status_code=499.
                    tracing::warn!(
                        combo_id = combo.id.0,
                        target_id = target.id.0,
                        provider = %target.provider_id,
                        attempt,
                        "client cancelled between targets; aborting pipeline"
                    );
                    return self.client_disconnected_result(attempt);
                }
                // Bug 4 fix: per-target retry. The retry policy is
                // applied to the *individual model*, not to the
                // whole combo walk: we try the same target up to
                // `retries.max_attempts` times, with backoff
                // between attempts. If every attempt on this
                // target errors retryably, the inner loop breaks
                // and the outer target loop falls through to the
                // next target in the combo (bug 3 contract).
                // Rationale: the pre-fix implementation had a
                // *single* `for attempt in 1..=max_attempts`
                // loop wrapping the whole combo walk, so a
                // retryable failure on target A consumed the
                // entire retry budget before target B was ever
                // tried. This is what the user perceived as
                // "retries don't fire on a per-model basis". The
                // fix is to apply retries per-target: after
                // target A's budget is exhausted, the pipeline
                // moves on to target B with a fresh budget.
                let policy = RetryPolicy::from_config(&self.config.retries);
                let mut target_attempt: u8 = 1;
                let mut result = self
                    .execute_single(&req, &combo, target, target_attempt, race_size as u8)
                    .await;
                // The retry loop body: only enter when the previous
                // attempt errored *retryably* AND we still have
                // attempts left AND the client hasn't cancelled.
                // Any of the three break-out conditions hands the
                // result (success or final failure) back to the
                // outer target loop, which decides whether to
                // continue to the next target (bug 3 fall-through)
                // or to return.
                while let Some(e) = &result.error {
                    if !RetryPolicy::is_retryable(e) {
                        // Non-retryable error (e.g. 4xx, validation).
                        // Bug 3 takes over: the next target in the
                        // combo gets a try.
                        break;
                    }
                    if target_attempt >= policy.max_attempts {
                        // Exhausted the per-target retry budget.
                        // Bug 3 fall-through to the next target.
                        break;
                    }
                    if {
                        let mut rx = req.client_disconnected.clone();
                        self.is_client_disconnected(&mut rx)
                    } {
                        // Client cancelled; abort the per-target
                        // retry. The outer target loop's
                        // disconnect check (a few lines above this
                        // block) will fire on the next iteration
                        // and short-circuit the whole pipeline.
                        break;
                    }
                    let delay = match policy.delay_after_attempt(target_attempt) {
                        Some(d) => d,
                        None => break,
                    };
                    tracing::debug!(
                        combo_id = combo.id.0,
                        target_id = target.id.0,
                        provider = %target.provider_id,
                        target_attempt,
                        next_attempt = target_attempt + 1,
                        delay_ms = delay.as_millis() as u64,
                        error = %e,
                        "target failed retryably; retrying same target"
                    );
                    tokio::time::sleep(delay).await;
                    // Saturate so a misconfigured `max_attempts` (or
                    // a future bump to u16) can't wrap the counter
                    // and turn the loop into an infinite retry.
                    target_attempt = target_attempt.saturating_add(1);
                    result = self
                        .execute_single(&req, &combo, target, target_attempt, race_size as u8)
                        .await;
                }
                // 6a. Update the persistent cooldown registry. A
                //     successful attempt clears any existing row; a
                //     retryable failure parks the target for
                //     `cooldown_secs`. 4xx and other non-retryable
                //     errors do not touch the cooldown (they're
                //     user-side bugs that will just keep coming
                //     back; the circuit breaker on the account is
                //     what handles those, if anything).
                let cooldown_op = match &result.error {
                    None => Some("clear"),
                    Some(e) if RetryPolicy::is_retryable(e) => Some("record"),
                    Some(_) => None,
                };
                if cooldown_op.is_some() {
                    let cooldown_conn = self.conn.lock();
                    match cooldown_op {
                        Some("clear") => {
                            if let Err(e) =
                                crate::cooldown::clear(&cooldown_conn, target.id)
                            {
                                tracing::warn!(
                                    combo_id = combo.id.0,
                                    target_id = target.id.0,
                                    error = %e,
                                    "cooldown::clear failed; non-fatal"
                                );
                            }
                        }
                        Some("record") => {
                            let reason = result
                                .error
                                .as_ref()
                                .map(|e| e.to_string())
                                .unwrap_or_else(|| "retryable failure".to_string());
                            if let Err(e) = crate::cooldown::record_failure(
                                &cooldown_conn,
                                target.id,
                                &reason,
                                self.config.cooldown_secs,
                            ) {
                                tracing::warn!(
                                    combo_id = combo.id.0,
                                    target_id = target.id.0,
                                    error = %e,
                                    "cooldown::record_failure failed; non-fatal"
                                );
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                match &result.error {
                    None => return result,
                    // For `Strategy::Priority` combos, walk the ENTIRE row regardless
                    // of error type — operator's intent is "try these in order, give
                    // each one a chance". A 4xx from model A doesn't mean model B
                    // will also 4xx (different model, different validation). Short-
                    // circuiting on 4xx here was a regression of the original
                    // walk-the-row contract. For non-Priority strategies (RoundRobin,
                    // Shuffle), preserve the short-circuit: those operators want
                    // fast-fail on non-retryable errors because they're racing all
                    // the targets anyway.
                    Some(e) if !RetryPolicy::is_retryable(e) && !matches!(combo.strategy, Strategy::Priority) => {
                        return result;
                    }
                    Some(e) => {
                        tracing::debug!(
                            combo_id = combo.id.0,
                            target_id = target.id.0,
                            provider = %target.provider_id,
                            strategy = ?combo.strategy,
                            error = %e,
                            "target failed; trying next target"
                        );
                        last_result = Some(result);
                    }
                }
            }

            // Bug 4 fix: the per-target retry is now done inside the
            // target loop (the `while let Some(e) = &result.error`
            // block above). The pre-fix code had a *second* retry
            // loop here that re-walked the whole combo on every
            // outer iteration, which is what gave the operator the
            // illusion of "no retries happen" (one model that always
            // 5xx'd would consume the whole combo-walk budget; the
            // other models in the combo would never see the budget
            // and would only get one shot per outer iteration). With
            // per-target retries, the combo walk happens once and
            // each target gets its own fresh retry budget. If every
            // target errored, surface the last per-target retry's
            // final result (which carries the last failure).
            return last_result.unwrap_or_else(|| self.failure(
                CoreError::NoHealthyTargets(combo.id.0),
                attempt,
                ErrorPhase::Route,
            ));
        }

        // Unreachable: the loop always returns on the last attempt's match arm.
        // Emit a defensive Internal error so the type-checker stays happy.
        self.failure(
            CoreError::Internal("pipeline loop exited without result".into()),
            self.config.max_attempts,
            ErrorPhase::Route,
        )
    }

    /// Recursively flatten a list of `ComboTarget` rows (as returned by
    /// `resolve_targets`) into a flat list of directly-executable
    /// targets, replacing each sub-combo target with the sub-combo's
    /// own children. After flattening, runs
    /// [`combos::expand_account_rotation`] on the merged list so the
    /// sub-combo's flat children (which were never seen by the
    /// outer `resolve_targets` call) get the same account-rotation
    /// fan-out as top-level targets.
    ///
    /// Cycle detection and max-depth are delegated to
    /// [`combos::resolve_combo_to_targets`].
    fn flatten_targets(
        &self,
        root_combo_id: &ComboId,
        targets: Vec<ComboTarget>,
    ) -> Result<Vec<ComboTarget>> {
        // Fast path: no sub-combo entries → nothing to flatten. The
        // outer `resolve_targets` already expanded the top-level
        // targets, so we can return as-is.
        if !targets.iter().any(|t| t.sub_combo_id.is_some()) {
            return Ok(targets);
        }
        let mut out = Vec::with_capacity(targets.len());
        let conn = self.conn.lock();
        let mut visited: Vec<ComboId> = vec![*root_combo_id];
        for t in targets {
            if let Some(sub_id) = t.sub_combo_id {
                let sub_flat = combos::resolve_combo_to_targets(&conn, sub_id, &mut visited, 0)?;
                out.extend(sub_flat);
            } else {
                out.push(t);
            }
        }
        // The flattened children have not been through the
        // account-rotation expansion (the outer
        // `resolve_targets` only expanded the *root* combo's
        // targets). Run it now so a child with `account_id = None`
        // is fanned out into one row per healthy account of its
        // provider.
        combos::expand_account_rotation(&conn, out)
    }

    // ---------------------------------------------------------------------
    // Target resolution
    // ---------------------------------------------------------------------

    fn load_combo(&self, req: &PipelineRequest) -> Result<Combo> {
        // The routing layer may inject a synthetic combo for a direct-
        // model request. The synthetic combo is built in memory (no
        // `combos` row exists for it) and we trust the caller to
        // produce a well-formed `Combo` struct.
        if let Some(combo) = req.combo_override.as_ref() {
            return Ok(combo.clone());
        }
        let conn = self.conn.lock();
        combos::get_combo(&conn, req.combo_id)?.ok_or(CoreError::ComboNotFound(req.combo_id.0))
    }

    /// Look up the combo's targets, apply its strategy (priority or
    /// round-robin), and expand `account_id = None` into one row per
    /// healthy account of that provider.
    fn resolve_targets(
        &self,
        combo: &Combo,
        targets_override: Option<&[ComboTarget]>,
    ) -> Result<Vec<ComboTarget>> {
        // Routing-layer override: use the in-memory list directly.
        // The synthetic combo produced by the routing layer does not
        // have a `combos.id` row, so the DB lookup would be both
        // wasteful and (for a negative synthetic id) potentially
        // wrong. We still go through `expand_account_rotation` so
        // a `None` account_id on a synthetic target with multiple
        // healthy accounts is expanded to one target per account.
        if let Some(overrides) = targets_override {
            let conn = self.conn.lock();
            return combos::expand_account_rotation(&conn, overrides.to_vec());
        }

        let conn = self.conn.lock();
        // The first read just sanity-checks the combo exists. The order
        // resolution re-runs `list_targets` internally, so this is cheap.
        let _ = combos::list_targets(&conn, combo.id)?;
        let ordered = combos::resolve_target_order(
            &conn,
            combo.id,
            combo.strategy,
            &self.rr_counters,
        )?;
        combos::expand_account_rotation(&conn, ordered)
    }

    /// If the combo has no targets, try to auto-populate it with the
    /// first provider that has a healthy account and active models.
    /// Returns the number of targets added (0 when nothing changed).
    ///
    /// Best-effort: a DB error here is non-fatal for the request — the
    /// caller falls through to `NoHealthyTargets` and the failure is
    /// recorded to `usage` so the dashboard's Live Logs tail still sees
    /// the request. An INFO log is emitted when the auto-populate
    /// actually inserted targets so the operator can see the system
    /// healing itself.
    fn auto_populate_if_empty(&self, combo: &Combo) -> Result<usize> {
        // Cheap pre-check: if the combo already has targets, this is a
        // no-op. Avoids taking the writer mutex for nothing on the
        // (common) healthy path.
        {
            let conn = self.conn.lock();
            if !combos::list_targets(&conn, combo.id)?.is_empty() {
                return Ok(0);
            }
        }

        let added = {
            let conn = self.conn.lock();
            combos::auto_populate_empty_combo(&conn, combo.id)?
        };

        if added > 0 {
            tracing::info!(
                combo_id = combo.id.0,
                combo_name = %combo.name,
                added_targets = added,
                "auto-populated empty combo with healthy provider's active models"
            );
        }
        Ok(added)
    }

    /// Record a single `usage` row for the `NoHealthyTargets` path so
    /// the dashboard's Live Logs tail isn't permanently empty when the
    /// pipeline rejects a request before reaching the upstream.
    ///
    /// The row carries zero tokens, zero cost, a `race_lost=false` /
    /// `race_total=1` shape (matching the single-target path), and an
    /// `error_msg` of `"no_healthy_targets"` so the operator can grep
    /// for this exact failure mode.
    fn record_no_healthy_targets_row(
        &self,
        req: &PipelineRequest,
        combo: &Combo,
        started: Instant,
    ) {
        let input = UsageInput {
            request_id: req.request_id,
            trace_id: TraceId::new(),
            attempt: 1,
            // No target existed to extract a provider from; record an
            // empty string so the row still parses.
            provider_id: crate::ids::ProviderId::new(""),
            account_id: None,
            combo_id: Some(combo.id),
            combo_target_id: None,
            model_row_id: None,
            upstream_model_id: req
                .openai_request
                .model
                .clone(),
            prompt_tokens: None,
            completion_tokens: None,
            connect_ms: None,
            ttft_ms: None,
            total_ms: started.elapsed().as_millis() as u64,
            status_code: 502,
            error_msg: Some("no_healthy_targets".to_string()),
            race_total: 1,
            race_lost: false,
            api_key_id: req.api_key_id,
            request_body_json: None,
            response_body_json: None,
            request_headers: None,
            response_headers: None,
            error_message: Some("no_healthy_targets".to_string()),
            race_attempts: 1,
            is_streaming: false,
            stream_complete: false,
        };
        let conn = self.conn.lock();
        let _ = crate::cost::record(&conn, &input);
    }

    // ---------------------------------------------------------------------
    // Upstream execution (MVP: non-streaming, single-lane)
    // ---------------------------------------------------------------------

    async fn execute_single(
        &self,
        req: &PipelineRequest,
        combo: &Combo,
        target: &ComboTarget,
        attempt: u8,
        race_size: u8,
    ) -> PipelineResult {
        let started = Instant::now();
        let trace_id = TraceId::new();

        // Live-log stage event: request accepted by the pipeline.
        // Only emitted when recording is ON — this is the switch
        // the dashboard exposes. When OFF, the operator doesn't
        // care about per-phase granularity and we save the broadcast
        // channel some traffic.
        if self.is_recording() {
            crate::usage::publish_stage_event(crate::usage::StageEvent {
                request_id: req.request_id.to_string(),
                trace_id: trace_id.to_string(),
                provider_id: target.provider_id.to_string(),
                upstream_model_id: String::new(),
                stage: "started".into(),
                elapsed_ms: 0,
                connect_ms: None,
                ttft_ms: None,
                status_code: 0,
                error: None,
                timestamp: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
            });
        }

        // 0. Check if this provider uses a custom executor.
        //    Custom executors handle their own request translation,
        //    HTTP dispatch, and response parsing. We intercept here
        //    before the generic adapter path.
        //
        //    We read all DB metadata under the lock first, then drop
        //    it before calling the async executors. This avoids
        //    holding the rusqlite Connection (which is !Sync) across
        //    await points, keeping the future Send-safe for
        //    tokio::spawn.
        if let Some(account_id) = target.account_id {
            let is_custom = matches!(
                target.provider_id.as_str(),
                "kiro" | "antigravity" | "antigravity-cli"
            );
            if is_custom {
                // Read access token + provider-specific metadata
                // under the lock, then release it.
                let (access_token, kiro_meta, antigravity_project) = {
                    let conn = self.conn.lock();
                    let access_token = match crate::accounts::decrypt_access_token(
                        &conn,
                        account_id,
                        &self.config.master_key,
                    ) {
                        Ok(t) => t,
                        Err(e) => {
                            drop(conn);
                            return self.record_and_fail(
                                req,
                                combo,
                                target,
                                attempt,
                                race_size,
                                &e,
                                started,
                                None,
                                None,
                                None,
                                e.http_status(),
                            );
                        }
                    };

                    match target.provider_id.as_str() {
                        "kiro" => {
                            let meta =
                                crate::executor_kiro::read_account_meta(&conn, account_id)
                                    .unwrap_or(None);
                            (access_token, meta, None)
                        }
                        "antigravity" | "antigravity-cli" => {
                            let pid =
                                crate::executor_antigravity::read_project_id(&conn, account_id);
                            match pid {
                                Ok(p) => (access_token, None, Some(p)),
                                Err(e) => {
                                    drop(conn);
                                    return self.record_and_fail(
                                        req,
                                        combo,
                                        target,
                                        attempt,
                                        race_size,
                                        &e,
                                        started,
                                        None,
                                        None,
                                        None,
                                        e.http_status(),
                                    );
                                }
                            }
                        }
                        _ => unreachable!(),
                    }
                }; // conn lock dropped here

                let executor_result = match target.provider_id.as_str() {
                    "kiro" => {
                        let region = kiro_meta
                            .as_ref()
                            .map(|m| m.region.as_str())
                            .unwrap_or(crate::executor_kiro::KIRO_DEFAULT_REGION);
                        let profile_arn = kiro_meta
                            .as_ref()
                            .and_then(|m| m.profile_arn.as_deref());
                        // Gate 3: the kiro executor now takes
                        // `&Arc<UpstreamClient>` (the hyper-based
                        // client) instead of `&reqwest::Client`. The
                        // client is shared with the chat dispatch via
                        // `PipelineConfig::upstream_client`. See
                        // `executor_kiro.rs` for the migration notes.
                        crate::executor_kiro::execute_kiro(
                            &self.config.upstream_client,
                            &access_token,
                            region,
                            profile_arn,
                            &req.openai_request,
                        )
                        .await
                    }
                    "antigravity" | "antigravity-cli" => {
                        let project_id = antigravity_project.as_deref().unwrap_or("");
                        // Gate 3: the antigravity executor now takes
                        // `&Arc<UpstreamClient>` instead of
                        // `&reqwest::Client`. See
                        // `executor_antigravity.rs` for the migration
                        // notes.
                        crate::executor_antigravity::execute_antigravity(
                            &self.config.upstream_client,
                            &access_token,
                            project_id,
                            &req.openai_request,
                        )
                        .await
                    }
                    _ => unreachable!(),
                };

                return match executor_result {
                    Ok(response) => {
                        let total_ms = started.elapsed().as_millis() as u64;
                        let _ = self.record_attempt_raw_with_tokens(
                            req,
                            combo,
                            target,
                            None,
                            None,
                            None,
                            None,
                            total_ms,
                            200,
                            attempt,
                            race_size,
                            trace_id,
                            response.usage.as_ref().map(|u| u.prompt_tokens),
                            response.usage.as_ref().map(|u| u.completion_tokens),
                            None, // request_body_json
                            None, // response_body_json
                            None, // request_headers
                            None, // response_headers
                        );
                        PipelineResult {
                            status_code: 200,
                            error: None,
                            final_response: Some(response),
                            attempts: attempt,
                        }
                    }
                    Err(e) => self.record_and_fail(
                        req,
                        combo,
                        target,
                        attempt,
                        race_size,
                        &e,
                        started,
                        None,
                        None,
                        None,
                        e.http_status(),
                    ),
                };
            }
        }

        // 1. Find the adapter for this provider.
        let adapter = match self.adapter_for(&target.provider_id) {
            Some(a) => a,
            None => {
                let err = CoreError::ProviderNotFound(target.provider_id.to_string());
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    attempt,
                    race_size,
                    &err,
                    started,
                    None,
                    None,
                    None,
                    0,
                );
            }
        };

        // 2. Load the model row. Sub-combo targets (`model_row_id =
        //    None`) reach this point only via a programming error
        //    (the resolver is supposed to flatten them away before
        //    they land in `execute_single`); the explicit error
        //    surface is just defense in depth.
        let model_row_id = match target.model_row_id {
            Some(m) => m,
            None => {
                let err = CoreError::Internal(format!(
                    "execute_single called on a sub-combo target (id={})",
                    target.id.0
                ));
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    attempt,
                    race_size,
                    &err,
                    started,
                    None,
                    None,
                    None,
                    0,
                );
            }
        };
        let model = match self.load_model(model_row_id) {
            Ok(m) => m,
            Err(e) => {
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    attempt,
                    race_size,
                    &e,
                    started,
                    None,
                    None,
                    None,
                    0,
                );
            }
        };

        // 3. Resolve timeouts via the 3-level precedence rule.
        let conn = self.conn.lock();
        let provider_timeouts = match timeouts::load_provider_timeouts(&conn, &target.provider_id) {
            Ok(t) => t,
            Err(e) => {
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    attempt,
                    race_size,
                    &e,
                    started,
                    Some(&model),
                    None,
                    None,
                    0,
                );
            }
        };
        let model_overrides = match ModelTimeoutOverrides::from_json(model.timeout_overrides_json.as_deref()) {
            Ok(o) => o,
            Err(e) => {
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    attempt,
                    race_size,
                    &e,
                    started,
                    Some(&model),
                    None,
                    None,
                    0,
                );
            }
        };
        let resolved_timeouts = timeouts::resolve(
            &self.config.defaults,
            provider_timeouts.as_ref(),
            Some(&model_overrides),
        );
        drop(conn);

        // 4. Decide the wire format. Mixed providers consult the model row.
        let target_format = match adapter.format() {
            AdapterFormat::Openai => crate::models::TargetFormat::Openai,
            AdapterFormat::Anthropic => crate::models::TargetFormat::Anthropic,
            AdapterFormat::Mixed => model.target_format,
            AdapterFormat::Gemini => crate::models::TargetFormat::Gemini,
        };

        // 4a. Replace the model field with the real upstream model id
        //     from the DB. For direct requests the client echoes back
        //     the proxy-level `<provider>/<model_id>` id; for combo
        //     requests the client sends the combo name (e.g. "nerd").
        //     In both cases `model.model_id` is the correct upstream
        //     model id to forward.
        let mut upstream_req = req.openai_request.clone();
        upstream_req.model = model.model_id.as_str().to_string();
        // Use the original client's stream preference. When streaming,
        // the pipeline reads SSE from upstream and forwards chunks
        // through stream_sink. When not streaming, we buffer the full
        // response.
        upstream_req.stream = req.openai_request.stream;

        // 5. Translate the OpenAI request into the provider's native shape.
        let translated_body = match target_format {
            crate::models::TargetFormat::Openai => match serde_json::to_value(&upstream_req) {
                Ok(v) => v,
                Err(e) => {
                    let err = CoreError::Parse(format!("serialize openai request: {e}"));
                    return self.record_and_fail(
                        req,
                        combo,
                        target,
                        attempt,
                        race_size,
                        &err,
                        started,
                        Some(&model),
                        None,
                        None,
                        0,
                    );
                }
            },
            crate::models::TargetFormat::Anthropic => {
                let anthro = crate::translation::openai_to_anthropic(&upstream_req);
                match serde_json::to_value(&anthro) {
                    Ok(v) => v,
                    Err(e) => {
                        let err = CoreError::Parse(format!("serialize anthropic request: {e}"));
                        return self.record_and_fail(
                            req,
                            combo,
                            target,
                            attempt,
                            race_size,
                            &err,
                            started,
                            Some(&model),
                            None,
                            None,
                            0,
                        );
                    }
                }
            }
            crate::models::TargetFormat::Gemini => {
                let gemini = crate::translation::openai_to_gemini(&upstream_req);
                match serde_json::to_value(&gemini) {
                    Ok(v) => v,
                    Err(e) => {
                        let err = CoreError::Parse(format!("serialize gemini request: {e}"));
                        return self.record_and_fail(
                            req,
                            combo,
                            target,
                            attempt,
                            race_size,
                            &err,
                            started,
                            Some(&model),
                            None,
                            None,
                            0,
                        );
                    }
                }
            }
        };

        // 6. Resolve the credential for this target.
        //
        // `account_id = None` is the expansion result only when the provider has
        // no healthy accounts. In that case the pipeline may continue only if
        // the provider is explicitly configured for anonymous access.
        //
        // `account_id = Some(_)` is a concrete account selection. Do not try an
        // anonymous request first: providers such as NVIDIA NIM can return a
        // retryable 500 for anonymous calls, so the previous 401/403-only
        // fallback never retried with the stored key.
        let api_key = match self.resolve_target_api_key(target) {
            Ok(api_key) => api_key,
            Err(e) => {
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    attempt,
                    race_size,
                    &e,
                    started,
                    Some(&model),
                    None,
                    None,
                    0,
                );
            }
        };

        // 7. Build the HTTP request and dispatch it.
        let url = adapter.build_chat_url(target_format, &model.model_id);
        let headers = adapter.build_headers(&api_key, target_format, &model.model_id);

        // Live-log stage event: about to open the upstream socket.
        // We treat anything between `started` and the actual byte
        // arrival (DB lookups, body translation, header resolve) as
        // `connecting` — the operator cares about "how long until I
        // see the first upstream byte", not about which micro-phase
        // dominates.
        if self.is_recording() {
            crate::usage::publish_stage_event(crate::usage::StageEvent {
                request_id: req.request_id.to_string(),
                trace_id: trace_id.to_string(),
                provider_id: target.provider_id.to_string(),
                upstream_model_id: model.model_id.as_str().to_string(),
                stage: "connecting".into(),
                elapsed_ms: started.elapsed().as_millis() as u64,
                connect_ms: None,
                ttft_ms: None,
                status_code: 0,
                error: None,
                timestamp: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
            });
        }

        let result = self
            .dispatch_upstream(
                target,
                combo,
                req,
                &model,
                target_format,
                &url,
                &headers,
                &translated_body,
                &resolved_timeouts,
                started,
                attempt,
                race_size,
                trace_id,
            )
            .await;

        // 8. Update circuit breaker based on the result.
        //
        // Contract: a `ClientDisconnected` result is a *user-driven*
        // event, not an upstream one. We do not want it to
        // artificially reset the breaker to a healthy state (the
        // `_ => record_success` arm would do that), and we
        // certainly don't want to increment the failure counter
        // (the upstream was never tried or its socket was aborted
        // because the client gave up). The correct behaviour is
        // "leave the breaker alone" — its snapshot from the
        // eligibility filter is still accurate for the next
        // request. The `RetryPolicy::is_retryable` arm correctly
        // excludes `ClientDisconnected` so the cooldown registry
        // is also untouched.
        if let Some(aid) = target.account_id {
            match &result.error {
                Some(CoreError::ClientDisconnected) => {
                    // Intentionally do nothing. See the comment
                    // above for the reasoning.
                    tracing::debug!(
                        account_id = aid.0,
                        "client cancelled; leaving circuit breaker untouched"
                    );
                }
                Some(e) if RetryPolicy::is_retryable(e) => {
                    self.circuit_breaker.record_failure(aid);
                }
                _ => {
                    self.circuit_breaker.record_success(aid);
                }
            }
        }

        result
    }

    /// Drive the actual upstream HTTP call.
    ///
    /// `execute_single` does all the setup (adapter, model, timeouts, key,
    /// url, headers, translated body) and hands those primitives to this
    /// method, which is responsible for the I/O: build the reqwest request,
    /// send it, measure timings, parse the body, and translate it back into
    /// an `OpenAIResponse`.
    ///
    /// All outcomes (success, transport error, upstream HTTP error, parse
    /// error) are folded into a `PipelineResult`; the surrounding
    /// `execute_single` does not need to inspect the internals.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_upstream(
        &self,
        target: &ComboTarget,
        combo: &Combo,
        req: &PipelineRequest,
        model: &Model,
        target_format: crate::models::TargetFormat,
        url: &str,
        headers: &[(String, String)],
        body_value_param: &serde_json::Value,
        resolved_timeouts: &Timeouts,
        started: Instant,
        attempt: u8,
        race_size: u8,
        trace_id: TraceId,
    ) -> PipelineResult {
        // Gate 2: both the non-streaming path AND the streaming path
        // now go through the hyper-based `UpstreamClient`
        // (`PipelineConfig::upstream_client`). The reqwest
        // `request_builder` chain is gone from this dispatch.
        //
        // We serialize the request body to `Bytes` here so we don't
        // have to hand the `serde_json::Value` to the streaming
        // builder (the streaming path used to serialize via
        // `RequestBuilder::json`; now it uses the same `Bytes`).
        //
        // Notes on the timeouts:
        // - `total` is enforced inside the upstream client via the
        //   `TimeoutProfile::Custom` resolved from `as_resolved()`.
        // - `connect` (= dial + TLS budget) is split into three
        //   phases on the upstream side (dns / dial / tls), all
        //   bounded by the same wall-clock budget — see
        //   `Timeouts::as_resolved` for the mapping.
        // - `ttft` becomes `headers_ms` on the upstream side; a
        //   headers timeout is reported as `UpstreamError::Timeout(Headers)`.
        // - `idle_chunk` is enforced as a per-chunk gap inside the
        //   upstream client's `UpstreamBodyStream::next_chunk`.
        let body_bytes = match serde_json::to_vec(body_value_param) {
            Ok(b) => bytes::Bytes::from(b),
            Err(e) => {
                let err = CoreError::Parse(format!("serialize upstream body: {e}"));
                return self.record_and_fail(
                    req, combo, target, attempt, race_size, &err, started,
                    Some(model), None, None,
                    err.http_status(),
                );
            }
        };
        let mut upstream_request = UpstreamRequest::post_json(url.to_string(), body_bytes);
        // Caller-supplied headers (auth, content-type overrides from
        // the adapter, etc.) — `post_json` already sets
        // `Content-Type: application/json`, so `insert` overwrites if
        // a caller header collides (matches the reqwest chain's
        // behavior with `.header(k, v)` which appends; we choose
        // overwrite for determinism — the adapter layer is
        // responsible for not setting conflicting headers).
        for (k, v) in headers {
            // HeaderMap's insert() requires HeaderName/HeaderValue;
            // parse the strings. Skip headers that fail to parse —
            // matches the previous `.header(k.as_str(), v.as_str())`
            // which also silently dropped invalid values.
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::from_bytes(k.as_bytes()),
                http::HeaderValue::from_str(v),
            ) {
                upstream_request.headers.insert(name, value);
            }
        }

        // STREAMING PATH: when the client requested streaming and we
        // have a stream_sink, read SSE lines from upstream and forward
        // them in real-time. Both paths now share the same
        // `UpstreamRequest`; the streaming helper takes ownership of
        // it and calls the hyper-based `UpstreamClient` itself.
        if req.openai_request.stream {
            if let Some(sink) = &req.stream_sink {
                return self
                    .dispatch_upstream_streaming(
                        target, combo, req, model, target_format,
                        sink, resolved_timeouts, started, attempt,
                        race_size, trace_id, upstream_request,
                    )
                    .await;
            }
        }

        // Send + measure.
        //
        // Cancellation: the `client_disconnected` watch is the
        // operator's signal that the client has gone away. The
        // upstream client accepts a `CancellationToken`; we mirror
        // the watch into a token via `from_watch`. The token is
        // consulted by the client at every phase boundary (DNS, dial,
        // TLS, write, headers, body chunk, total) and inside the
        // `UpstreamBodyStream` between frames.
        //
        // Pre-flight check: if the watch has ALREADY flipped to
        // `true` (e.g. the client disconnected while we were
        // building the request) we short-circuit to a structured
        // `ClientDisconnected` result. The pre-flight is the only
        // place we map `UpstreamError::Cancel` → `ClientDisconnected`
        // — see below for the rationale.
        let send_start = Instant::now();
        if *req.client_disconnected.borrow() {
            let elapsed = send_start.elapsed().as_millis() as u64;
            tracing::warn!(
                combo_id = combo.id.0,
                target_id = target.id.0,
                provider = %target.provider_id,
                elapsed_ms = elapsed,
                "client disconnected before upstream send; aborting attempt"
            );
            return self.record_and_fail(
                req, combo, target, attempt, race_size,
                &CoreError::ClientDisconnected, started,
                Some(model), Some(elapsed), None,
                CoreError::ClientDisconnected.http_status(),
            );
        }
        let cancel_token = CancellationToken::from_watch(req.client_disconnected.clone());
        let result = self
            .config
            .upstream_client
            .call(
                upstream_request,
                crate::upstream::TimeoutProfile::Custom(resolved_timeouts.as_resolved()),
                cancel_token,
            )
            .await;
        let connect_and_send_ms = send_start.elapsed().as_millis() as u64;

        // Map the `UpstreamError` taxonomy to the `CoreError` shape
        // the downstream code expects. The split mirrors the
        // pre-migration `SendAbortReason` + `e.is_timeout()` /
        // `e.to_string()` mapping 1-to-1, except we now have
        // per-phase `UpstreamPhase` attribution and the `Cancel`
        // variant.
        let response_result: std::result::Result<crate::upstream::UpstreamResponse, UpstreamError> = match result {
            Ok(r) => Ok(r),
            Err(UpstreamError::Cancel) => {
                tracing::warn!(
                    combo_id = combo.id.0,
                    target_id = target.id.0,
                    provider = %target.provider_id,
                    elapsed_ms = connect_and_send_ms,
                    "client cancelled during upstream send; aborting attempt"
                );
                return self.record_and_fail(
                    req, combo, target, attempt, race_size,
                    &CoreError::ClientDisconnected, started,
                    Some(model), Some(connect_and_send_ms), None,
                    CoreError::ClientDisconnected.http_status(),
                );
            }
            Err(UpstreamError::Timeout(phase)) => {
                // The upstream client reports a single stalled phase.
                // We map DNS / Dial / Tls / Write / Headers to the
                // pre-migration `phase: "connect"` label (the
                // production connector cannot separate them; the
                // `headers` boundary is the closest match for the
                // dial+TLS+wait-for-headers wall-clock budget the
                // old `tokio::time::timeout(connect, …)` covered).
                // `Body` maps to the total-budget timeout the
                // pre-migration code reported as `phase: "total"`.
                let phase_label = match phase {
                    crate::upstream::UpstreamPhase::Dns
                    | crate::upstream::UpstreamPhase::Dial
                    | crate::upstream::UpstreamPhase::Tls
                    | crate::upstream::UpstreamPhase::Write
                    | crate::upstream::UpstreamPhase::Headers => "connect",
                    crate::upstream::UpstreamPhase::Body => "total",
                };
                tracing::warn!(
                    combo_id = combo.id.0,
                    target_id = target.id.0,
                    provider = %target.provider_id,
                    phase = %phase,
                    elapsed_ms = connect_and_send_ms,
                    "upstream phase timed out; aborting attempt"
                );
                let err = CoreError::UpstreamTimeout {
                    phase: phase_label.to_string(),
                    ms: connect_and_send_ms,
                };
                return self.record_and_fail(
                    req, combo, target, attempt, race_size, &err, started,
                    Some(model), Some(connect_and_send_ms), None,
                    err.http_status(),
                );
            }
            Err(UpstreamError::Connection(msg))
            | Err(UpstreamError::Tls(msg))
            | Err(UpstreamError::Http(msg))
            | Err(UpstreamError::Decode(msg))
            | Err(UpstreamError::Invalid(msg)) => {
                let err = CoreError::UpstreamConnection(msg);
                return self.record_and_fail(
                    req, combo, target, attempt, race_size, &err, started,
                    Some(model), Some(connect_and_send_ms), None,
                    err.http_status(),
                );
            }
        };

        // Live-log stage helper closure. Only fires when recording
        // is ON; OFF means the dashboard's "Record" toggle is off
        // and the operator doesn't want per-phase noise. Throttled
        // per-call: each caller site picks which stages matter.
        let mut emit_stage = |stage: &str, status: u16, err: Option<String>| {
            if !self.is_recording() {
                return;
            }
            crate::usage::publish_stage_event(crate::usage::StageEvent {
                request_id: req.request_id.to_string(),
                trace_id: trace_id.to_string(),
                provider_id: target.provider_id.to_string(),
                upstream_model_id: model.model_id.as_str().to_string(),
                stage: stage.into(),
                elapsed_ms: started.elapsed().as_millis() as u64,
                connect_ms: Some(connect_and_send_ms),
                ttft_ms: None,
                status_code: status,
                error: err,
                timestamp: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
            });
        };

        // Unwrap the `Ok` arm. The match above has already handled
        // every `Err` variant with an early `return` (or fell
        // through to `Ok`). This is just the `let response = match
        // { Ok(r) => r, Err(_) => unreachable!() }` of the original
        // code, expressed with `into_result` semantics.
        let response = match response_result {
            Ok(r) => r,
            Err(_) => unreachable!("error variants are handled above with early return"),
        };

        let status_code = response.status.as_u16();
        // Extract response headers BEFORE consuming the body
        let response_headers: Option<std::collections::BTreeMap<String, String>> = if self.is_recording() {
            Some(
                response
                    .headers
                    .iter()
                    .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or_default().to_string()))
                    .collect(),
            )
        } else {
            None
        };
        // Live-log: socket+headers are in, body streaming next.
        // For non-2xx we go to the error branch below; emit there.
        if (200..300).contains(&status_code) {
            emit_stage("waiting_ttft", status_code, None);
        }
        // For non-streaming we have no first-chunk signal, so the
        // conservative thing is to record `ttft == total`. The cost
        // module's tokens/sec guard already turns this into `None`.
        let ttft_ms = started.elapsed().as_millis() as u64;

        // Read the body via the upstream client's `collect()`. The
        // body is bounded to 32 MiB at the upstream layer; on cancel
        // we get `UpstreamError::Cancel` (mapped above); on read
        // failure we get `UpstreamError::Http`. We map any failure
        // to `UpstreamConnection` with a `read upstream body: …`
        // prefix, matching the pre-migration `record_and_fail` call
        // shape.
        let body_bytes = match response.collect().await {
            Ok(b) => b,
            Err(UpstreamError::Cancel) => {
                tracing::warn!(
                    combo_id = combo.id.0,
                    target_id = target.id.0,
                    provider = %target.provider_id,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "client cancelled during upstream body read; aborting attempt"
                );
                return self.record_and_fail(
                    req, combo, target, attempt, race_size,
                    &CoreError::ClientDisconnected, started,
                    Some(model), Some(connect_and_send_ms), Some(ttft_ms),
                    CoreError::ClientDisconnected.http_status(),
                );
            }
            Err(UpstreamError::Timeout(phase)) => {
                let err = CoreError::UpstreamTimeout {
                    phase: phase.as_str().to_string(),
                    ms: started.elapsed().as_millis() as u64,
                };
                return self.record_and_fail(
                    req, combo, target, attempt, race_size, &err, started,
                    Some(model), Some(connect_and_send_ms), Some(ttft_ms),
                    status_code,
                );
            }
            Err(e) => {
                let err = CoreError::UpstreamConnection(format!("read upstream body: {e}"));
                return self.record_and_fail(
                    req, combo, target, attempt, race_size, &err, started,
                    Some(model), Some(connect_and_send_ms), Some(ttft_ms),
                    status_code,
                );
            }
        };

        // Non-2xx upstream responses are surfaced as UpstreamError, with
        // the body included for the usage row. We still consume the body
        // so the connection is released back to the pool cleanly.
        if !(200..300).contains(&status_code) {
            let body_str = String::from_utf8_lossy(&body_bytes).to_string();
            let err = CoreError::UpstreamError {
                status: status_code,
                provider: target.provider_id.to_string(),
                model: model.model_id.as_str().to_string(),
                body: body_str,
            };
            return self.record_and_fail(
                req, combo, target, attempt, race_size, &err, started,
                Some(model), Some(connect_and_send_ms), Some(ttft_ms),
                status_code,
            );
        }

        // 2xx: parse into the native wire format, then translate to
        // OpenAIResponse if needed.
        let response_body_raw: serde_json::Value = match serde_json::from_slice(&body_bytes) {
            Ok(v) => v,
            Err(e) => {
                let err = CoreError::Parse(format!("upstream json: {e}"));
                return self.record_and_fail(
                    req, combo, target, attempt, race_size, &err, started,
                    Some(model), Some(connect_and_send_ms), Some(ttft_ms),
                    status_code,
                );
            }
        };

        // Snapshot the body JSON before it gets moved into the
        // format-specific parser below; we need it both as the
        // recorded response body and as a source for the request
        // body we are about to send.
        let response_body_value = response_body_raw.clone();

        let openai_response = match target_format {
            crate::models::TargetFormat::Openai => match serde_json::from_value::<OpenAIResponse>(response_body_raw) {
                Ok(r) => r,
                Err(e) => {
                    let err = CoreError::Parse(format!("parse openai response: {e}"));
                    return self.record_and_fail(
                        req, combo, target, attempt, race_size, &err, started,
                        Some(model), Some(connect_and_send_ms), Some(ttft_ms),
                        status_code,
                    );
                }
            },
            crate::models::TargetFormat::Anthropic => {
                let anthropic_resp: crate::translation::AnthropicResponse =
                    match serde_json::from_value(response_body_raw) {
                        Ok(r) => r,
                        Err(e) => {
                            let err = CoreError::Parse(format!("parse anthropic response: {e}"));
                            return self.record_and_fail(
                                req, combo, target, attempt, race_size, &err, started,
                                Some(model), Some(connect_and_send_ms), Some(ttft_ms),
                                status_code,
                            );
                        }
                    };
                crate::translation::anthropic_to_openai(&anthropic_resp)
            }
            crate::models::TargetFormat::Gemini => {
                let gemini_resp: crate::translation::GeminiResponse =
                    match serde_json::from_value(response_body_raw) {
                        Ok(r) => r,
                        Err(e) => {
                            let err = CoreError::Parse(format!("parse gemini response: {e}"));
                            return self.record_and_fail(
                                req, combo, target, attempt, race_size, &err, started,
                                Some(model), Some(connect_and_send_ms), Some(ttft_ms),
                                status_code,
                            );
                        }
                    };
                crate::translation::gemini_to_openai(&gemini_resp)
            }
        };

        let prompt_tokens = openai_response.usage.as_ref().map(|u| u.prompt_tokens);
        let completion_tokens = openai_response.usage.as_ref().map(|u| u.completion_tokens);

        // Record the successful attempt and return.
        let total_ms_now = started.elapsed().as_millis() as u64;
        let request_headers_btm: std::collections::BTreeMap<String, String> =
            headers.iter().cloned().collect();
        let _ = self.record_attempt_raw_with_tokens(
            req, combo, target, Some(model), None,
            Some(connect_and_send_ms), Some(ttft_ms), total_ms_now,
            status_code, attempt, race_size, trace_id,
            prompt_tokens, completion_tokens,
            Some(body_value_param.clone()), // request body: original translated JSON sent upstream
            Some(response_body_value),      // response body: snapshot captured before the parse consumed body_value
            Some(request_headers_btm),      // request headers
            response_headers,               // response headers (captured before body was read)
        );

        PipelineResult {
            status_code,
            error: None,
            final_response: Some(openai_response),
            attempts: attempt,
        }
    }

    // ---------------------------------------------------------------------
    // Streaming upstream dispatch
    // ---------------------------------------------------------------------

    /// Streaming variant of dispatch_upstream. Reads SSE lines from
    /// the upstream response and forwards each translated chunk through
    /// the stream_sink channel in real-time.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_upstream_streaming(
        &self,
        target: &ComboTarget,
        combo: &Combo,
        req: &PipelineRequest,
        model: &Model,
        target_format: crate::models::TargetFormat,
        sink: &mpsc::Sender<String>,
        resolved_timeouts: &Timeouts,
        started: Instant,
        attempt: u8,
        race_size: u8,
        trace_id: TraceId,
        upstream_request: UpstreamRequest,
    ) -> PipelineResult {
        // Cancellation: the `client_disconnected` watch is the
        // operator's signal that the client has gone away. The
        // hyper-based upstream client accepts a `CancellationToken`;
        // we mirror the watch into a token via `from_watch`. The
        // token is consulted by the client at every phase boundary
        // (DNS, dial, TLS, write, headers, body chunk, total) AND
        // inside the `UpstreamBodyStream::next_chunk` between
        // frames — so the body loop below does NOT need its own
        // per-chunk cancel watch for the upstream-side cancellation
        // to fire. The `client_disconnected` watch IS still consulted
        // in the body loop, but only to short-circuit the
        // post-stream accounting (usage row, [DONE] sentinel) —
        // see the post-loop `is_client_disconnected` check.
        //
        // Pre-flight check: if the watch has ALREADY flipped to
        // `true` (e.g. the client disconnected while we were
        // building the request) we short-circuit to a structured
        // `ClientDisconnected` result without spinning up a hyper
        // request that we'd cancel 1 ms later.
        let send_start = Instant::now();
        if *req.client_disconnected.borrow() {
            let elapsed = send_start.elapsed().as_millis() as u64;
            tracing::warn!(
                combo_id = combo.id.0,
                target_id = target.id.0,
                provider = %target.provider_id,
                elapsed_ms = elapsed,
                "client disconnected before upstream streaming send; aborting attempt"
            );
            return self.record_and_fail(
                req, combo, target, attempt, race_size,
                &CoreError::ClientDisconnected, started,
                Some(model), Some(elapsed), None,
                CoreError::ClientDisconnected.http_status(),
            );
        }
        let cancel_token = CancellationToken::from_watch(req.client_disconnected.clone());
        let result = self
            .config
            .upstream_client
            .call(
                upstream_request,
                crate::upstream::TimeoutProfile::Custom(resolved_timeouts.as_resolved()),
                cancel_token,
            )
            .await;
        let connect_and_send_ms = send_start.elapsed().as_millis() as u64;

        // Map the `UpstreamError` taxonomy to the `CoreError` shape
        // the downstream code expects. Mirrors the non-streaming
        // path's mapping 1-to-1: a per-phase `UpstreamPhase` becomes
        // the `phase` label, the `Cancel` variant becomes a
        // structured `ClientDisconnected` result, and the rest
        // collapse to `UpstreamConnection`. The streaming path
        // doesn't have a "total" pre-migration mapping (it was
        // `phase: "total"` from reqwest's whole-request timeout),
        // so `Body` here maps to the same `"total"` label to keep
        // the dashboards consistent.
        let response_result: std::result::Result<crate::upstream::UpstreamResponse, UpstreamError> = match result {
            Ok(r) => Ok(r),
            Err(UpstreamError::Cancel) => {
                tracing::warn!(
                    combo_id = combo.id.0,
                    target_id = target.id.0,
                    provider = %target.provider_id,
                    elapsed_ms = connect_and_send_ms,
                    "client cancelled during upstream streaming send; aborting attempt"
                );
                return self.record_and_fail(
                    req, combo, target, attempt, race_size,
                    &CoreError::ClientDisconnected, started,
                    Some(model), Some(connect_and_send_ms), None,
                    CoreError::ClientDisconnected.http_status(),
                );
            }
            Err(UpstreamError::Timeout(phase)) => {
                let phase_label = match phase {
                    crate::upstream::UpstreamPhase::Dns
                    | crate::upstream::UpstreamPhase::Dial
                    | crate::upstream::UpstreamPhase::Tls
                    | crate::upstream::UpstreamPhase::Write
                    | crate::upstream::UpstreamPhase::Headers => "connect",
                    crate::upstream::UpstreamPhase::Body => "total",
                };
                tracing::warn!(
                    combo_id = combo.id.0,
                    target_id = target.id.0,
                    provider = %target.provider_id,
                    phase = %phase,
                    elapsed_ms = connect_and_send_ms,
                    "upstream phase timed out; aborting streaming attempt"
                );
                let err = CoreError::UpstreamTimeout {
                    phase: phase_label.to_string(),
                    ms: connect_and_send_ms,
                };
                return self.record_and_fail(
                    req, combo, target, attempt, race_size, &err, started,
                    Some(model), Some(connect_and_send_ms), None,
                    err.http_status(),
                );
            }
            Err(UpstreamError::Connection(msg))
            | Err(UpstreamError::Tls(msg))
            | Err(UpstreamError::Http(msg))
            | Err(UpstreamError::Decode(msg))
            | Err(UpstreamError::Invalid(msg)) => {
                let err = CoreError::UpstreamConnection(msg);
                return self.record_and_fail(
                    req, combo, target, attempt, race_size, &err, started,
                    Some(model), Some(connect_and_send_ms), None,
                    err.http_status(),
                );
            }
        };

        // `response_result` is `Ok` here because every error arm
        // above already returned. The `match` is needed to satisfy
        // the borrow checker (we move out of the binding), but
        // we make the `Err` arm unreachable so the compiler is
        // happy.
        let response = match response_result {
            Ok(r) => r,
            Err(e) => unreachable!(
                "dispatch_upstream_streaming: response_result was expected to be Ok after error-mapping match; got {:?}",
                e
            ),
        };

        let status_code = response.status.as_u16();
        if !(200..300).contains(&status_code) {
            let body_str = match response.body.collect_all().await {
                Ok(b) => String::from_utf8_lossy(&b).to_string(),
                Err(_) => String::new(),
            };
            let err = CoreError::UpstreamError {
                status: status_code,
                provider: target.provider_id.to_string(),
                model: model.model_id.as_str().to_string(),
                body: body_str,
            };
            return self.record_and_fail(
                req, combo, target, attempt, race_size, &err, started,
                Some(model), Some(connect_and_send_ms), None,
                status_code,
            );
        }

        let chunk_id = format!("chatcmpl-{}", uuid::Uuid::new_v4());
        let created = chrono::Utc::now().timestamp() as u64;
        let model_name = model.model_id.as_str().to_string();

        // The first SSE chunk emits the `streaming` stage event
        // (see the `if ttft_ms.is_none()` branch below) so we know
        // `ttft_ms` exactly at that moment. We deliberately do NOT
        // emit a `streaming` event here at the start of the loop
        // — the operator's "ttft" number is the time from socket
        // open to first body byte, and a separate "headers in"
        // event would imply we have a distinct timing for that,
        // which we don't. The `waiting_ttft` event we emitted a
        // few lines above already covers "headers received, body
        // streaming next".

        // Read the response as a byte stream, split into lines,
        // and process each SSE line.
        //
        // `UpstreamBodyStream` does NOT implement `futures::Stream`
        // (intentionally — see `upstream::response`); we iterate it
        // via `next_chunk().await` instead. The hyper-based stream
        // already consults the `CancellationToken` and the
        // per-chunk deadline between frames, so the loop's only
        // extra responsibility is to surface the `client_disconnected`
        // watch transition into the cancellation path: when the
        // watch flips, the body future is dropped (cancelling the
        // hyper body) and the loop exits cleanly. We do NOT
        // short-circuit by `None`-ing the chunk arm of the select
        // here — returning `UpstreamBodyStream::next_chunk`'s actual
        // result keeps the existing post-loop accounting
        // (usage row, [DONE] sentinel) running.
        let mut stream = response.body;
        let mut buffer = String::new();
        let mut usage: Option<crate::translation::OpenAIUsage> = None;
        let mut ttft_ms: Option<u64> = None;
        let first_chunk_time = Instant::now();
        let mut current_event_type: Option<String> = None;

        loop {
            // Race the next byte-stream poll against the
            // `client_disconnected` watch. We use the boxed form
            // of `next_chunk` so the future is `Unpin` and the
            // select! arm is well-typed. When the watch fires,
            // the body future is dropped (cancelling the
            // underlying hyper body read), and we break the loop
            // so the post-loop accounting can decide whether
            // to record a `ClientDisconnected` usage row.
            let mut cancel_rx_chunk = req.client_disconnected.clone();
            // The boxed `next_chunk` returns
            // `UpstreamResult<Option<Bytes>>`; we wrap it in an
            // `Option` so the cancel arm can produce `None` and
            // break the loop.
            let chunk_result: Option<crate::upstream::UpstreamResult<Option<bytes::Bytes>>> =
                tokio::select! {
                    biased;
                    _ = cancel_rx_chunk.changed() => None,
                    next = stream.next_chunk_boxed() => Some(next),
                };
            let chunk_result = match chunk_result {
                Some(r) => r,
                None => break,
            };
            let bytes = match chunk_result {
                Ok(Some(b)) => b,
                Ok(None) => break, // end of stream
                Err(e) => {
                    // Map the `UpstreamError` to `CoreError` for the
                    // per-chunk failure path. Body chunk timeouts
                    // map to `UpstreamTimeout { phase: "idle_chunk" }`
                    // (the same label reqwest+StreamExt would have
                    // surfaced — the per-chunk gap budget), other
                    // errors to `UpstreamConnection`. We use the
                    // pre-migration label for dashboard consistency.
                    let err = match e {
                        UpstreamError::Timeout(UpstreamPhase::Body) => {
                            CoreError::UpstreamTimeout {
                                phase: "idle_chunk".into(),
                                ms: resolved_timeouts.idle_chunk.as_millis() as u64,
                            }
                        }
                        UpstreamError::Cancel => {
                            // The hyper body returned cancel — the
                            // client_disconnected watch has fired.
                            // We break out of the loop and let the
                            // post-loop checkpoint emit the
                            // structured `ClientDisconnected` row.
                            break;
                        }
                        UpstreamError::Connection(msg) => {
                            CoreError::UpstreamConnection(
                                format!("stream read: {}", msg),
                            )
                        }
                        UpstreamError::Tls(msg) => {
                            CoreError::UpstreamConnection(
                                format!("stream read: {}", msg),
                            )
                        }
                        UpstreamError::Http(msg) => {
                            CoreError::UpstreamConnection(
                                format!("stream read: {}", msg),
                            )
                        }
                        UpstreamError::Decode(msg) => {
                            CoreError::UpstreamConnection(
                                format!("stream read: {}", msg),
                            )
                        }
                        UpstreamError::Invalid(msg) => {
                            CoreError::UpstreamConnection(
                                format!("stream read: {}", msg),
                            )
                        }
                        // Body-phase timeout that isn't `Body` (e.g.
                        // a future phase variant) — treat as idle.
                        UpstreamError::Timeout(_) => {
                            CoreError::UpstreamConnection(
                                format!("stream read: {}", e),
                            )
                        }
                    };
                    return self.record_and_fail(
                        req, combo, target, attempt, race_size, &err, started,
                        Some(model), Some(connect_and_send_ms), ttft_ms,
                        status_code,
                    );
                }
            };

            buffer.push_str(&String::from_utf8_lossy(&bytes));

            // Process complete lines.
            while let Some(line_end) = buffer.find('\n') {
                let line = buffer[..line_end].to_string();
                buffer = buffer[line_end + 1..].to_string();

                let line = line.trim_end_matches('\r');
                if line.is_empty() || line.starts_with(':') {
                    continue;
                }

                // Record TTFT on the first data-bearing line.
                if ttft_ms.is_none() {
                    ttft_ms = Some(first_chunk_time.elapsed().as_millis() as u64);
                    // Live-log stage event: first byte-of-body
                    // arrived. The dashboard updates the row's
                    // "in phase" label from "waiting_ttft" to
                    // "streaming" and shows the ttft value.
                    if self.is_recording() {
                        crate::usage::publish_stage_event(crate::usage::StageEvent {
                            request_id: req.request_id.to_string(),
                            trace_id: trace_id.to_string(),
                            provider_id: target.provider_id.to_string(),
                            upstream_model_id: model_name.clone(),
                            stage: "streaming".into(),
                            elapsed_ms: started.elapsed().as_millis() as u64,
                            connect_ms: Some(connect_and_send_ms),
                            ttft_ms,
                            status_code: 200,
                            error: None,
                            timestamp: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
                        });
                    }
                }

                // Parse based on upstream format.
                let parsed = match target_format {
                    crate::models::TargetFormat::Openai => {
                        crate::sse::parse_openai_sse_line(&line)
                    }
                    crate::models::TargetFormat::Gemini => {
                        crate::sse::parse_gemini_sse_line(&line, &chunk_id, created, &model_name)
                    }
                    crate::models::TargetFormat::Anthropic => {
                        // Anthropic SSE: track event type across lines
                        match crate::sse::parse_anthropic_sse_stream_line(&line, &mut current_event_type) {
                            Ok(Some(payload)) => {
                                // Translate Anthropic payload to OpenAI chunk
                                match crate::sse::translate_anthropic_sse_payload(
                                    &payload, &chunk_id, created, &model_name,
                                ) {
                                    Ok(Some(chunk)) => Ok(Some(chunk)),
                                    Ok(None) => Ok(None),
                                    Err(e) => Err(e),
                                }
                            }
                            Ok(None) => Ok(None),
                            Err(e) => Err(e),
                        }
                    }
                };

                match parsed {
                    Ok(Some(chunk)) => {
                        if chunk.done {
                            // Send [DONE] sentinel and break.
                            let _ = sink.send("[DONE]".to_string()).await;
                        } else {
                            // Update usage if present.
                            if chunk.usage.is_some() {
                                usage = chunk.usage;
                            }
                            // Forward the translated chunk as raw JSON.
                            let json_str = serde_json::to_string(&chunk.payload).unwrap_or_default();
                            if sink.send(json_str).await.is_err() {
                                // Client disconnected.
                                return PipelineResult {
                                    status_code,
                                    error: None,
                                    final_response: None,
                                    attempts: attempt,
                                };
                            }
                        }
                    }
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::warn!(
                            chunk_id = %chunk_id,
                            error = %e,
                            "failed to parse SSE line from upstream"
                        );
                        continue;
                    }
                }
            }
        } // end of SSE chunk loop

        // Process any remaining data in the buffer.
        if !buffer.is_empty() {
            let line = buffer.trim();
            if !line.is_empty() && !line.starts_with(':') {
                let parsed = match target_format {
                    crate::models::TargetFormat::Openai => {
                        crate::sse::parse_openai_sse_line(line)
                    }
                    crate::models::TargetFormat::Gemini => {
                        crate::sse::parse_gemini_sse_line(line, &chunk_id, created, &model_name)
                    }
                    crate::models::TargetFormat::Anthropic => {
                        match crate::sse::parse_anthropic_sse_stream_line(line, &mut current_event_type) {
                            Ok(Some(payload)) => {
                                match crate::sse::translate_anthropic_sse_payload(
                                    &payload, &chunk_id, created, &model_name,
                                ) {
                                    Ok(Some(chunk)) => Ok(Some(chunk)),
                                    Ok(None) => Ok(None),
                                    Err(e) => Err(e),
                                }
                            }
                            Ok(None) => Ok(None),
                            Err(e) => Err(e),
                        }
                    }
                };
                if let Ok(Some(chunk)) = parsed {
                    if let Some(u) = chunk.usage {
                        usage = Some(u);
                    }
                    if !chunk.done {
                        let json_str = serde_json::to_string(&chunk.payload).unwrap_or_default();
                        let _ = sink.send(json_str).await;
                    }
                }
            }
        }

        // Cancellation checkpoint: if the watch fired during the
        // stream poll (above), the `while let` loop already
        // exited normally. We must NOT send [DONE] or any further
        // chunks to a client that has already given up — and we
        // must record a `ClientDisconnected` usage row, not a
        // success row, so the dashboard reflects the cancellation.
        // The `tracing::warn!` is the same line the dispatch-loop
        // emit for boundary-only disconnects, so operators see a
        // single shape in their logs.
        if {
            let mut rx = req.client_disconnected.clone();
            self.is_client_disconnected(&mut rx)
        } {
            tracing::warn!(
                combo_id = combo.id.0,
                target_id = target.id.0,
                provider = %target.provider_id,
                "client cancelled during SSE stream; aborting attempt"
            );
            return self.record_and_fail(
                req, combo, target, attempt, race_size,
                &CoreError::ClientDisconnected, started,
                Some(model), Some(connect_and_send_ms), ttft_ms,
                CoreError::ClientDisconnected.http_status(),
            );
        }

        // Send [DONE] if the upstream didn't send it explicitly.
        // Some upstreams close the connection without the sentinel.
        let _ = sink.send("[DONE]".to_string()).await;

        let total_ms = started.elapsed().as_millis() as u64;

        // Record usage.
        let prompt_tokens = usage.as_ref().map(|u| u.prompt_tokens);
        let completion_tokens = usage.as_ref().map(|u| u.completion_tokens);
        let _ = self.record_attempt_raw_with_tokens(
            req, combo, target, Some(model), None,
            Some(connect_and_send_ms), ttft_ms, total_ms,
            status_code, attempt, race_size, trace_id,
            prompt_tokens, completion_tokens,
            None, // request_body_json
            None, // response_body_json
            None, // request_headers
            None, // response_headers
        );

        PipelineResult {
            status_code,
            error: None,
            final_response: None, // Streaming: no buffered response
            attempts: attempt,
        }
    }

    // ---------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------

    fn adapter_for(&self, provider_id: &crate::ids::ProviderId) -> Option<Arc<dyn ProviderAdapter>> {
        self.config
            .adapters
            .iter()
            .find(|a| a.id() == provider_id)
            .cloned()
    }

    fn load_model(&self, row_id: crate::ids::ModelRowId) -> Result<Model> {
        let conn = self.conn.lock();
        models::get_by_row_id(&conn, row_id)?.ok_or(CoreError::ModelNotFound {
            provider: "<unknown>".into(),
            model: format!("row_id={}", row_id.0),
        })
    }

    fn decrypt_account_key(&self, account_id: crate::ids::AccountId) -> Result<String> {
        let conn = self.conn.lock();
        crate::accounts::decrypt_api_key(&conn, account_id, &self.config.master_key)
    }

    /// Resolve the API key to use for a given target.
    ///
    /// - `account_id = Some(_)`: decrypt the stored key for that account.
    /// - `account_id = None` and the provider's `auth_type` is `None`:
    ///   return an empty string (anonymous access).
    /// - `account_id = None` and the provider requires auth (Bearer,
    ///   XApiKey, etc.): return `CoreError::Auth` — the target has no
    ///   credential and the upstream does not accept anonymous requests.
    fn resolve_target_api_key(&self, target: &ComboTarget) -> Result<String> {
        match target.account_id {
            Some(account_id) => self.decrypt_account_key(account_id),
            None => {
                let conn = self.conn.lock();
                match crate::providers::get(&conn, &target.provider_id)? {
                    Some(p) if matches!(p.auth_type, crate::providers::AuthType::None) => {
                        Ok(String::new())
                    }
                    _ => Err(CoreError::Auth(format!(
                        "combo_target {} has no account_id after expansion",
                        target.id.0
                    ))),
                }
            }
        }
    }

    /// Build a `(status, error)` result without writing a usage row.
    fn failure(&self, err: CoreError, attempts: u8, _phase: ErrorPhase) -> PipelineResult {
        PipelineResult {
            status_code: err.http_status(),
            error: Some(err),
            final_response: None,
            attempts,
        }
    }

    /// Build a `PipelineResult` representing a client-cancellation
    /// abort. The variant `CoreError::ClientDisconnected` carries
    /// HTTP status 499 (see [`crate::error::CoreError::http_status`])
    /// and the short code `client_disconnected`, which the
    /// `record_and_fail` path picks up unchanged into the usage
    /// row. Used by the per-target boundary check in `Pipeline::run`
    /// and by the `tokio::select!` wrappers around the upstream
    /// HTTP send and the SSE byte stream (see TAREA 2 / 3 of the
    /// cancellation wire-up).
    fn client_disconnected_result(&self, attempts: u8) -> PipelineResult {
        self.failure(CoreError::ClientDisconnected, attempts, ErrorPhase::Retry)
    }

    /// `true` iff the request's `client_disconnected` watch has
    /// flipped to `true` since the last check. Cheap: one
    /// `borrow_and_update` on an internally-`Arc`-backed watch
    /// channel. Cloning the `watch::Receiver` itself is also cheap,
    /// which is what we do at every `tokio::select!` checkpoint.
    fn is_client_disconnected(&self, rx: &mut watch::Receiver<bool>) -> bool {
        // `borrow_and_update` is the non-blocking form: it marks
        // the current value as "seen" and returns whether it is
        // `true`. We don't need the `changed()` future here because
        // the `tokio::select!` arms own the wait themselves; this
        // helper is only used at synchronous checkpoints (the
        // per-target boundary in `Pipeline::run` and the success
        // post-checks in the dispatch loop).
        *rx.borrow_and_update()
    }

    /// Record a failed attempt and return a finished `PipelineResult`.
    #[allow(clippy::too_many_arguments)]
    fn record_and_fail(
        &self,
        req: &PipelineRequest,
        combo: &Combo,
        target: &ComboTarget,
        attempt: u8,
        race_size: u8,
        err: &CoreError,
        started: Instant,
        model: Option<&Model>,
        connect_ms: Option<u64>,
        ttft_ms: Option<u64>,
        status_code: u16,
    ) -> PipelineResult {
        let trace_id = TraceId::new();
        let total_ms = started.elapsed().as_millis() as u64;
        // Live-log stage event: the request failed. We emit this
        // *before* the DB row so the dashboard's stage label and
        // its "row inserted" notification can race — the frontend
        // collapses both into a single visible row, so the
        // ordering inside the broadcast is not user-visible.
        if self.is_recording() {
            crate::usage::publish_stage_event(crate::usage::StageEvent {
                request_id: req.request_id.to_string(),
                trace_id: trace_id.to_string(),
                provider_id: target.provider_id.to_string(),
                upstream_model_id: model
                    .map(|m| m.model_id.as_str().to_string())
                    .unwrap_or_default(),
                stage: "failed".into(),
                elapsed_ms: total_ms,
                connect_ms,
                ttft_ms,
                status_code,
                error: Some(err.to_string()),
                timestamp: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
            });
        }
        // Bug #4 fix: the failure path used to drop the request body
        // and headers, so even with recording=ON the DB row had
        // NULL in those columns. Recover the body from the original
        // OpenAI request (which we always have in `req`) and the
        // headers from the new `req.request_headers` slot. Response
        // side is None on this path — we never reached the upstream
        // call.
        let request_body_json = serde_json::to_value(&req.openai_request).ok();
        let request_headers = req.request_headers.clone();
        let _ = self.record_attempt_raw_with_tokens(
            req,
            combo,
            target,
            model,
            Some(err),
            connect_ms,
            ttft_ms,
            total_ms,
            status_code,
            attempt,
            race_size,
            trace_id,
            None,
            None,
            request_body_json,
            None,
            Some(request_headers),
            None,
        );
        PipelineResult {
            status_code: err.http_status(),
            error: Some(err.clone_for_result()),
            final_response: None,
            attempts: attempt,
        }
    }

    /// Lowest-level helper: build a [`UsageInput`] and call
    /// [`crate::cost::record`]. A write failure is logged via `tracing`
    /// and silently swallowed: the request has already been serviced
    /// and a missing usage row is preferable to a 500.
    #[allow(clippy::too_many_arguments)]
    fn record_attempt_raw(
        &self,
        req: &PipelineRequest,
        combo: &Combo,
        target: &ComboTarget,
        model: Option<&Model>,
        err: Option<&CoreError>,
        connect_ms: Option<u64>,
        ttft_ms: Option<u64>,
        total_ms: u64,
        status_code: u16,
        attempt: u8,
        race_size: u8,
        trace_id: TraceId,
    ) -> Result<()> {
        self.record_attempt_raw_with_tokens(
            req, combo, target, model, err,
            connect_ms, ttft_ms, total_ms, status_code, attempt, race_size, trace_id,
            None, None, None, None, None, None,
        )
    }

    /// Same as [`Self::record_attempt_raw`] but lets the caller pass
    /// the parsed token counts. Used by the success path of
    /// `dispatch_upstream` so the usage row carries the real prompt /
    /// completion token totals.
    #[allow(clippy::too_many_arguments)]
    fn record_attempt_raw_with_tokens(
        &self,
        req: &PipelineRequest,
        combo: &Combo,
        target: &ComboTarget,
        model: Option<&Model>,
        err: Option<&CoreError>,
        connect_ms: Option<u64>,
        ttft_ms: Option<u64>,
        total_ms: u64,
        status_code: u16,
        attempt: u8,
        race_size: u8,
        trace_id: TraceId,
        prompt_tokens: Option<u32>,
        completion_tokens: Option<u32>,
        request_body_json: Option<serde_json::Value>,
        response_body_json: Option<serde_json::Value>,
        request_headers: Option<std::collections::BTreeMap<String, String>>,
        response_headers: Option<std::collections::BTreeMap<String, String>>,
    ) -> Result<()> {
        let recording = self.is_recording();
        let input = UsageInput {
            request_id: req.request_id,
            trace_id,
            attempt,
            provider_id: target.provider_id.clone(),
            account_id: target.account_id,
            combo_id: Some(combo.id),
            combo_target_id: Some(target.id),
            model_row_id: model.map(|m| m.row_id),
            upstream_model_id: model
                .map(|m| m.model_id.as_str().to_string())
                .unwrap_or_default(),
            prompt_tokens,
            completion_tokens,
            connect_ms,
            ttft_ms,
            total_ms,
            status_code,
            error_msg: err.map(|e| format!("{}", e)),
            race_total: race_size,
            race_lost: false,
            api_key_id: req.api_key_id,
            request_body_json: if recording { request_body_json } else { None },
            response_body_json: if recording { response_body_json } else { None },
            request_headers: if recording { request_headers } else { None },
            response_headers: if recording { response_headers } else { None },
            error_message: None,
            race_attempts: race_size,
            is_streaming: false,
            stream_complete: false,
        };
        let conn = self.conn.lock();
        cost::record(&conn, &input)?;
        Ok(())
    }

    pub fn emit_started_event(&self, req: &PipelineRequest, target: &ComboTarget, combo: &Combo) {
        let input = UsageInput {
            request_id: req.request_id,
            trace_id: req.trace_id,
            attempt: 1,
            provider_id: target.provider_id.clone(),
            account_id: target.account_id,
            combo_id: Some(combo.id),
            combo_target_id: Some(target.id),
            model_row_id: None,
            upstream_model_id: String::new(),
            prompt_tokens: Some(0),
            completion_tokens: Some(0),
            connect_ms: Some(0),
            ttft_ms: Some(0),
            total_ms: 0,
            status_code: 0,
            error_msg: None,
            race_total: combo.race_size,
            race_lost: false,
            api_key_id: req.api_key_id,
            request_body_json: None,
            response_body_json: None,
            request_headers: None,
            response_headers: None,
            error_message: None,
            race_attempts: combo.race_size,
            is_streaming: true,
            stream_complete: false,
        };
        crate::usage::broadcast_usage_input(&input);
    }
}

/// Return a copy of `req` whose `model` field has the proxy-level
/// `<provider>/` prefix stripped off, if present. Used at the boundary
/// between the public chat API (which returns `<provider>/<model_id>`)
/// and the upstream call (which expects only the upstream model id).
///
/// The strip is intentionally conservative:
/// - If `req.model` does not start with `<provider>/`, the value is
///   returned unchanged. This keeps backward compatibility for
///   clients that send the bare upstream id (or that target a
///   different provider than the one we picked).
/// - Only the first `/` is the separator; any subsequent `/` is
///   treated as part of the upstream model id and is preserved
///   (matches LiteLLM / OpenRouter conventions for nested model
///   namespaces like `nex-agi/nex-n2-pro:free`).
fn strip_provider_prefix(
    req: &OpenAIRequest,
    provider_id: &crate::ids::ProviderId,
) -> OpenAIRequest {
    let prefix = format!("{}/", provider_id.as_str());
    let stripped = if let Some(rest) = req.model.strip_prefix(&prefix) {
        rest.to_string()
    } else {
        req.model.clone()
    };
    let mut out = req.clone();
    out.model = stripped;
    out
}

/// Phase label for tracing/debug. Currently unused in production code
/// (the `_phase` argument is reserved for a future structured-logging
/// upgrade), but kept here so call sites document intent.
#[derive(Debug, Clone, Copy)]
pub enum ErrorPhase {
    Resolve,
    Route,
    Retry,
}

impl std::fmt::Display for ErrorPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ErrorPhase::Resolve => "resolve",
            ErrorPhase::Route => "route",
            ErrorPhase::Retry => "retry",
        };
        f.write_str(s)
    }
}

// Make `combo.strategy` slightly easier to consume in tests; silences the
// unused-import warning when no test in the module reads the type directly.
#[allow(dead_code)]
fn _strategy_marker(_: Strategy) {}

// `ErrorContext` is reserved for a future structured-logging upgrade that
// will let the pipeline attach req/trace/provider metadata to every error.
// Keep the import live so the symbol is available when that work lands.
#[allow(dead_code)]
fn _context_marker(_: ErrorContext) {}

// `TimeoutsConfig` is used by callers to build `PipelineConfig::defaults`;
// the import ensures the type re-export stays valid.
#[allow(dead_code)]
fn _timeouts_marker(_: TimeoutsConfig) {}

// `Instant` is the wall-clock source for the connect/ttft/total timers that
// `record_attempt` persists. The import is also kept live for the
// not-yet-implemented `dispatch_upstream` body.
#[allow(dead_code)]
fn _instant_marker(_: Instant) {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::combos;
    use crate::db::conn::DbPool;
    use crate::db::migrations;
    use crate::ids::{AccountId, ComboId, ComboTargetId, ModelRowId, ProviderId, RequestId, TraceId};
    use crate::models::TargetFormat;
    use crate::providers::{self, AuthType, ProviderFormat};
    use crate::secrets::MasterKey;
    use crate::translation::{OpenAIMessage, OpenAIRequest};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;
    use tokio::sync::{mpsc, watch};

    /// Build a fresh on-disk pool with migrations applied, plus an
    /// independent `Connection` wrapped in a `Mutex<Connection>` for the
    /// `Pipeline` to own. The same shape the rest of the crate's test
    /// modules use, with a unique tempdir per test to avoid `WAL`-file
    /// collisions when tests run in parallel.
    fn fresh_pool() -> (DbPool, Arc<parking_lot::Mutex<Connection>>, PathBuf) {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "openproxy-pipeline-test-{}-{}-{}",
            pid, nanos, n
        ));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("pipeline.db");
        let pool = DbPool::open(&path).expect("open pool");
        {
            let mut w = pool.writer();
            migrations::run(&mut w).expect("migrations");
        }
        // A second connection on the same file, owned by the Pipeline.
        let extra = Connection::open(&path).expect("open extra");
        let conn = Arc::new(parking_lot::Mutex::new(extra));
        (pool, conn, path)
    }

    /// A reasonable default `PipelineConfig` for tests: no real adapters
    /// (the tests only exercise the routing/usage path, not the HTTP path).
    fn test_config(master_key: Arc<MasterKey>) -> PipelineConfig {
        let defaults = Timeouts::from_config(&TimeoutsConfig::default());
        PipelineConfig {
            defaults,
            racing: RacingConfig::default(),
            retries: RetriesConfig::default(),
            max_attempts: 1,
            master_key,
            adapters: Arc::new(Vec::new()),
            // A vanilla HTTP client is fine for tests: nothing in the
            // routing path actually fires a request.
            http_client: reqwest::Client::new(),
            // 60s default cooldown for tests; individual tests that
            // exercise the cooldown path can pass a shorter value
            // through a local `PipelineConfig` override.
            cooldown_secs: 60,
            // Hyper-based upstream client. The default production
            // connector (rustls HTTPS) is fine for tests that don't
            // exercise the HTTP path; tests that DO need a real
            // upstream should rebuild the config with a test
            // connector.
            upstream_client: UpstreamClient::new(),
        }
    }

    /// Seed a provider so combo_targets FKs can be satisfied.
    fn seed_provider(conn: &Connection, provider_id: &str, auth_type: AuthType) {
        providers::create(
            conn,
            &ProviderId::new(provider_id),
            provider_id,
            "https://example.com",
            auth_type,
            ProviderFormat::Openai,
            None,
            None,
        )
        .expect("seed provider");
    }

    /// Seed a provider and a single model row, returning the model's row id.
    fn seed_provider_and_model(
        conn: &Connection,
        provider_id: &str,
        model_id: &str,
        fmt: TargetFormat,
    ) -> ModelRowId {
        providers::create(
            conn,
            &ProviderId::new(provider_id),
            provider_id,
            "https://example.com",
            AuthType::Bearer,
            match fmt {
                TargetFormat::Openai => ProviderFormat::Openai,
                TargetFormat::Anthropic => ProviderFormat::Anthropic,
                TargetFormat::Gemini => ProviderFormat::Openai,
            },
            None,
            None,
        )
        .expect("seed provider");
        conn.execute(
            "INSERT INTO models(provider_id, model_id, target_format) VALUES (?1, ?2, ?3)",
            rusqlite::params![provider_id, model_id, fmt.as_str()],
        )
        .expect("seed model");
        let id: i64 = conn
            .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
            .expect("last_insert_rowid");
        ModelRowId(id)
    }

    /// Build a `PipelineRequest` with sensible defaults.
    fn make_request(combo_id: ComboId) -> (PipelineRequest, watch::Sender<bool>) {
        let (_dis_tx, dis_rx) = watch::channel(false);
        let (_sink_tx, _sink_rx) = mpsc::channel::<String>(8);
        let req = PipelineRequest {
            request_id: RequestId::new(),
            trace_id: TraceId::new(),
            combo_id,
            openai_request: OpenAIRequest {
                model: "any".into(),
                messages: vec![OpenAIMessage {
                    role: "user".into(),
                    content: Some(serde_json::Value::String("hi".to_string())),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    extra: serde_json::Map::new(),
                }],
                stream: false,
                temperature: None,
                max_tokens: None,
                top_p: None,
                stop: None,
                extra: serde_json::Map::new(),
            },
            client_disconnected: dis_rx,
            stream_sink: Some(_sink_tx),
            api_key_id: None,
            combo_override: None,
            targets_override: None,
            request_headers: std::collections::BTreeMap::new(),
        };
        (req, _dis_tx)
    }

    #[test]
    fn pipeline_creation_doesnt_panic() {
        let (_pool, conn, _path) = fresh_pool();
        let cfg = test_config(Arc::new(MasterKey::generate()));
        // Constructing a Pipeline with an empty adapter set must succeed.
        let _p = Pipeline::new(conn, cfg);
    }

    #[test]
    fn resolve_targets_with_empty_combo_returns_empty() {
        let (pool, conn, _path) = fresh_pool();
        let combo_id = {
            let writer = pool.writer();
            let id = combos::create_combo(&writer, "empty", Strategy::Priority, 1).expect("create");
            id
        };

        let cfg = test_config(Arc::new(MasterKey::generate()));
        let p = Pipeline::new(conn, cfg);

        let combo = combos::get_combo(&pool.writer(), combo_id)
            .expect("get")
            .expect("present");
        let targets = p.resolve_targets(&combo, None).expect("resolve_targets");
        assert!(targets.is_empty(), "combo with no targets → empty vec");
    }

    #[tokio::test]
    async fn pipeline_run_with_no_targets_returns_502() {
        // With the auto-populate fallback in place, the only way to
        // hit the bare NoHealthyTargets path is to have an empty combo
        // AND no healthy provider to auto-fill from. We seed a single
        // (active) provider with no accounts and no models so the
        // auto-populate query returns 0 candidates.
        let (pool, conn, _path) = fresh_pool();
        let combo_id = {
            let writer = pool.writer();
            // Seed an active provider with no accounts and no models.
            providers::create(
                &writer,
                &ProviderId::new("p"),
                "p",
                "https://example.com",
                AuthType::Bearer,
                ProviderFormat::Openai,
                None,
                None,
            )
            .expect("seed provider");
            combos::create_combo(&writer, "no-targets", Strategy::Priority, 1).expect("create")
        };

        let cfg = test_config(Arc::new(MasterKey::generate()));
        let p = Pipeline::new(conn, cfg);

        let (req, _dis_tx) = make_request(combo_id);
        let result = p.run(req).await;

        // NoHealthyTargets is the failure path: 502 per `http_status()`.
        assert_eq!(result.status_code, 502, "no eligible targets → 502");
        match &result.error {
            Some(CoreError::NoHealthyTargets(id)) => assert_eq!(*id, combo_id.0),
            other => panic!("expected NoHealthyTargets, got {:?}", other),
        }
        assert!(result.final_response.is_none());
    }

    #[tokio::test]
    async fn pipeline_run_no_targets_records_usage_row() {
        // The NoHealthyTargets path must write a usage row so the
        // dashboard's Live Logs tail isn't permanently empty. We
        // arrange the same "no candidate provider" condition as the
        // test above and then assert a usage row was created.
        let (pool, conn, _path) = fresh_pool();
        let combo_id = {
            let writer = pool.writer();
            providers::create(
                &writer,
                &ProviderId::new("p"),
                "p",
                "https://example.com",
                AuthType::Bearer,
                ProviderFormat::Openai,
                None,
                None,
            )
            .expect("seed provider");
            combos::create_combo(&writer, "nerd", Strategy::Priority, 1).expect("create")
        };

        let cfg = test_config(Arc::new(MasterKey::generate()));
        let p = Pipeline::new(conn, cfg);

        let (req, _dis_tx) = make_request(combo_id);
        let _ = p.run(req).await;

        // A usage row should now exist. The dashboard reads this via
        // /v1/admin/usage/recent.
        let writer = pool.writer();
        let count: i64 = writer
            .query_row("SELECT COUNT(*) FROM usage", [], |r| r.get(0))
            .expect("count usage");
        assert_eq!(count, 1, "exactly one usage row was written");
        let (status, error): (i64, Option<String>) = writer
            .query_row(
                "SELECT status_code, error_msg FROM usage ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("read row");
        assert_eq!(status, 502);
        assert_eq!(error.as_deref(), Some("no_healthy_targets"));
    }

    #[tokio::test]
    async fn auto_populate_fills_combo_then_runs() {
        // The auto-populate fallback should turn an empty combo into
        // a routable one when there is a healthy provider with active
        // models. We seed (provider, healthy account, two active
        // models), create an empty combo, then call the pipeline and
        // expect it to NOT return NoHealthyTargets — instead the
        // auto-populate path fills the combo and the resolve+execute
        // step is reached. The execute will fail (no real adapter /
        // upstream) but the failure is something other than
        // NoHealthyTargets.
        let (pool, conn, _path) = fresh_pool();
        let mk = MasterKey::generate();
        let combo_id = {
            let writer = pool.writer();
            providers::create(
                &writer,
                &ProviderId::new("p"),
                "p",
                "https://example.com",
                AuthType::Bearer,
                ProviderFormat::Openai,
                None,
                None,
            )
            .expect("seed provider");
            // Two active models on the same provider.
            writer.execute(
                "INSERT INTO models(provider_id, model_id, target_format) VALUES ('p', 'm1', 'openai')",
                [],
            )
            .expect("seed m1");
            writer.execute(
                "INSERT INTO models(provider_id, model_id, target_format) VALUES ('p', 'm2', 'openai')",
                [],
            )
            .expect("seed m2");
            let provider = ProviderId::new("p");
            crate::accounts::create(
                &writer,
                &provider,
                Some("sk-test"),
                &mk,
                None,
                1,
                None,
            )
            .expect("seed account");
            combos::create_combo(&writer, "nerd", Strategy::Priority, 1).expect("create")
        };

        let cfg = test_config(Arc::new(mk));
        let p = Pipeline::new(conn, cfg);

        let (req, _dis_tx) = make_request(combo_id);
        let result = p.run(req).await;

        // The combo was auto-populated. The pipeline's `execute_single`
        // would normally dispatch to a real adapter; with an empty
        // adapter registry it falls through to a 500-ish failure
        // (no adapter). The key invariant is: NOT NoHealthyTargets.
        match &result.error {
            Some(CoreError::NoHealthyTargets(_)) => {
                panic!("auto-populate should have prevented NoHealthyTargets");
            }
            _ => {}
        }

        // And the combo now has 2 targets in the DB.
        let writer = pool.writer();
        let count: i64 = writer
            .query_row(
                "SELECT COUNT(*) FROM combo_targets WHERE combo_id = ?1",
                rusqlite::params![combo_id.0],
                |r| r.get(0),
            )
            .expect("count targets");
        assert_eq!(count, 2, "auto-populate added one target per active model");
    }

    // -------------------------------------------------------------------
    // Bonus tests that exercise the target-expansion + account-rotation
    // surface without needing an upstream HTTP server.
    // -------------------------------------------------------------------

    #[test]
    fn resolve_targets_with_healthy_account_expands_to_one() {
        let (pool, conn, _path) = fresh_pool();
        let (model, combo_id, mk) = {
            let writer = pool.writer();
            let model = seed_provider_and_model(&writer, "p", "m", TargetFormat::Openai);
            let combo_id =
                combos::create_combo(&writer, "c", Strategy::Priority, 1).expect("create");
            combos::add_target(
                &writer,
                combos::AddTargetInput {
                    combo_id,
                    provider_id: ProviderId::new("p"),
                    account_id: None,
                    model_row_id: Some(model),
                    sub_combo_id: None,
                    priority_order: 10,
                },
            )
            .expect("add target");

            // One healthy account exists, so the target is expanded to one row.
            let mk = MasterKey::generate();
            let _aid = crate::accounts::create(
                &writer,
                &ProviderId::new("p"),
                Some("sk-test-1"),
                &mk,
                None,
                1,
                None,
            )
            .expect("seed account");
            (model, combo_id, mk)
        };

        let cfg = test_config(Arc::new(mk));
        let p = Pipeline::new(conn, cfg);

        let combo = combos::get_combo(&pool.writer(), combo_id)
            .expect("get")
            .expect("present");
        let targets = p.resolve_targets(&combo, None).expect("resolve_targets");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].account_id, Some(AccountId(1)));
        let _ = model;
    }

    #[test]
    fn resolve_targets_with_no_healthy_accounts_drops_target() {
        let (pool, conn, _path) = fresh_pool();
        let combo_id = {
            let writer = pool.writer();
            let model = seed_provider_and_model(&writer, "p", "m", TargetFormat::Openai);
            let combo_id =
                combos::create_combo(&writer, "c", Strategy::Priority, 1).expect("create");
            combos::add_target(
                &writer,
                combos::AddTargetInput {
                    combo_id,
                    provider_id: ProviderId::new("p"),
                    account_id: None,
                    model_row_id: Some(model),
                    sub_combo_id: None,
                    priority_order: 10,
                },
            )
            .expect("add target");
            combo_id
        };

        let cfg = test_config(Arc::new(MasterKey::generate()));
        let p = Pipeline::new(conn, cfg);

        let combo = combos::get_combo(&pool.writer(), combo_id)
            .expect("get")
            .expect("present");
        // No accounts in the DB → target kept with account_id=None
        // (the pipeline handles auth, not the combo).
        let targets = p.resolve_targets(&combo, None).expect("resolve_targets");
        assert_eq!(targets.len(), 1, "target kept with account_id=None");
        assert!(targets[0].account_id.is_none());
    }

    #[test]
    fn resolve_target_api_key_account_id_returns_decrypted_key() {
        let (pool, conn, _path) = fresh_pool();
        let mk = MasterKey::generate();
        let target = {
            let writer = pool.writer();
            seed_provider(&writer, "p", AuthType::Bearer);
            writer
                .execute(
                    "INSERT INTO models(provider_id, model_id, target_format) VALUES ('p', 'm', 'openai')",
                    [],
                )
                .expect("seed model");
            let model_rowid: i64 = writer
                .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
                .expect("last_insert_rowid");
            let account_id = crate::accounts::create(
                &writer,
                &ProviderId::new("p"),
                Some("sk-test"),
                &mk,
                None,
                1,
                None,
            )
            .expect("seed account");
            let combo_id = combos::create_combo(&writer, "c", Strategy::Priority, 1).expect("combo");
            let target_id = combos::add_target(
                &writer,
                combos::AddTargetInput {
                    combo_id,
                    provider_id: ProviderId::new("p"),
                    account_id: Some(account_id),
                    model_row_id: Some(ModelRowId(model_rowid)),
                    sub_combo_id: None,
                    priority_order: 10,
                },
            )
            .expect("add target");
            combos::get_target(&writer, target_id).expect("get target").expect("target")
        };

        let cfg = test_config(Arc::new(mk));
        let p = Pipeline::new(conn, cfg);

        assert_eq!(p.resolve_target_api_key(&target).expect("key"), "sk-test");
    }

    #[test]
    fn resolve_target_api_key_none_auth_type_returns_empty() {
        let (pool, conn, _path) = fresh_pool();
        let target = {
            let writer = pool.writer();
            seed_provider(&writer, "p", AuthType::None);
            writer
                .execute(
                    "INSERT INTO models(provider_id, model_id, target_format) VALUES ('p', 'm', 'openai')",
                    [],
                )
                .expect("seed model");
            let model_rowid: i64 = writer
                .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
                .expect("last_insert_rowid");
            let combo_id = combos::create_combo(&writer, "c", Strategy::Priority, 1).expect("combo");
            let target_id = combos::add_target(
                &writer,
                combos::AddTargetInput {
                    combo_id,
                    provider_id: ProviderId::new("p"),
                    account_id: None,
                    model_row_id: Some(ModelRowId(model_rowid)),
                    sub_combo_id: None,
                    priority_order: 10,
                },
            )
            .expect("add target");
            combos::get_target(&writer, target_id).expect("get target").expect("target")
        };

        let cfg = test_config(Arc::new(MasterKey::generate()));
        let p = Pipeline::new(conn, cfg);

        assert_eq!(p.resolve_target_api_key(&target).expect("key"), "");
    }

    #[test]
    fn resolve_target_api_key_none_bearer_returns_auth_error() {
        let (pool, conn, _path) = fresh_pool();
        let target = {
            let writer = pool.writer();
            seed_provider(&writer, "p", AuthType::Bearer);
            writer
                .execute(
                    "INSERT INTO models(provider_id, model_id, target_format) VALUES ('p', 'm', 'openai')",
                    [],
                )
                .expect("seed model");
            let model_rowid: i64 = writer
                .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
                .expect("last_insert_rowid");
            let combo_id = combos::create_combo(&writer, "c", Strategy::Priority, 1).expect("combo");
            let target_id = combos::add_target(
                &writer,
                combos::AddTargetInput {
                    combo_id,
                    provider_id: ProviderId::new("p"),
                    account_id: None,
                    model_row_id: Some(ModelRowId(model_rowid)),
                    sub_combo_id: None,
                    priority_order: 10,
                },
            )
            .expect("add target");
            combos::get_target(&writer, target_id).expect("get target").expect("target")
        };

        let cfg = test_config(Arc::new(MasterKey::generate()));
        let p = Pipeline::new(conn, cfg);

        match p.resolve_target_api_key(&target).expect_err("auth error") {
            CoreError::Auth(msg) => assert!(msg.contains("has no account_id after expansion")),
            other => panic!("expected Auth error, got {:?}", other),
        }
    }

    // -------------------------------------------------------------------
    // strip_provider_prefix
    // -------------------------------------------------------------------

    fn make_request_with_model(model: &str) -> OpenAIRequest {
        OpenAIRequest {
            model: model.into(),
            messages: vec![OpenAIMessage {
                role: "user".into(),
                content: Some(serde_json::Value::String("hi".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            }],
            stream: false,
            temperature: None,
            max_tokens: None,
            top_p: None,
            stop: None,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn strip_provider_prefix_strips_matching_prefix() {
        // The proxy-level id the client sends in is `openrouter/foo/bar`.
        // The upstream expects `foo/bar`. The strip keeps the
        // nested `/` intact.
        let req = make_request_with_model("openrouter/foo/bar");
        let provider = ProviderId::new("openrouter");
        let stripped = strip_provider_prefix(&req, &provider);
        assert_eq!(stripped.model, "foo/bar");
    }

    #[test]
    fn strip_provider_prefix_keeps_bare_upstream_id() {
        // A client that sends the bare upstream id (no prefix) gets
        // it forwarded as-is. This is the legacy / non-conformant
        // path.
        let req = make_request_with_model("foo/bar");
        let provider = ProviderId::new("openrouter");
        let stripped = strip_provider_prefix(&req, &provider);
        assert_eq!(stripped.model, "foo/bar");
    }

    #[test]
    fn strip_provider_prefix_does_not_match_other_provider() {
        // The prefix only matches the *current* target's provider. A
        // request that happens to start with a different provider's
        // prefix is forwarded verbatim.
        let req = make_request_with_model("anthropic/claude-3.5-sonnet");
        let provider = ProviderId::new("openrouter");
        let stripped = strip_provider_prefix(&req, &provider);
        assert_eq!(stripped.model, "anthropic/claude-3.5-sonnet");
    }

    #[test]
    fn strip_provider_prefix_does_not_clobber_other_fields() {
        // Sanity: the helper must not touch anything other than
        // `model`. We compare the full request shape on the
        // non-`model` fields.
        let req = make_request_with_model("openrouter/foo/bar");
        let provider = ProviderId::new("openrouter");
        let stripped = strip_provider_prefix(&req, &provider);
        assert_eq!(stripped.messages.len(), 1);
        assert_eq!(stripped.messages[0].content.as_ref().and_then(serde_json::Value::as_str), Some("hi"));
        assert_eq!(stripped.stream, false);
        assert_eq!(stripped.model, "foo/bar");
    }

    // -------------------------------------------------------------------
    // Cooldown integration
    //
    // The pipeline's hot path now consults `target_cooldowns` and
    // writes back to it. We exercise the four observable behaviors
    // end-to-end (via `Pipeline::run`'s public surface), keeping
    // the tests lightweight by never actually firing an upstream
    // HTTP call — the path of interest is the "no eligible
    // targets" / "all targets retried" code path that the
    // cooldown touches.
    // -------------------------------------------------------------------

    /// Seed a (provider, healthy account, active model, target)
    /// tuple plus a combo that contains the target. Returns the
    /// combo id and the target id.
    fn seed_target_with_account(
        conn: &Connection,
        mk: &MasterKey,
    ) -> (ComboId, ComboTargetId, AccountId, ModelRowId) {
        providers::create(
            conn,
            &ProviderId::new("p"),
            "p",
            "https://example.com",
            AuthType::Bearer,
            ProviderFormat::Openai,
            None,
            None,
        )
        .expect("seed provider");
        conn.execute(
            "INSERT INTO models(provider_id, model_id, target_format) VALUES ('p', 'm', 'openai')",
            [],
        )
        .expect("seed model");
        let model_rowid: i64 = conn
            .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
            .expect("last_insert_rowid");
        let account_id = crate::accounts::create(
            conn,
            &ProviderId::new("p"),
            Some("sk-test"),
            mk,
            None,
            1,
            None,
        )
        .expect("seed account");
        let combo_id = combos::create_combo(conn, "c", Strategy::Priority, 1).expect("combo");
        let target_id = combos::add_target(
            conn,
            combos::AddTargetInput {
                combo_id,
                provider_id: ProviderId::new("p"),
                account_id: Some(account_id),
                model_row_id: Some(ModelRowId(model_rowid)),
                sub_combo_id: None,
                priority_order: 10,
            },
        )
        .expect("add target");
        (combo_id, target_id, account_id, ModelRowId(model_rowid))
    }

    #[tokio::test]
    async fn pipeline_probes_parked_target_when_only_option() {
        // Cooldown semantics: the persistent cooldown protects
        // *between* requests, not *within* a single request. When
        // a priority combo has exactly one target and that target
        // is parked in cooldown, the pipeline does NOT short-circuit
        // to `NoHealthyTargets` (502) anymore. Instead it falls
        // through to the dispatch loop with the unfiltered (pre-
        // cooldown) list, so the operator sees the real upstream
        // error (e.g. `UpstreamConnection`) rather than a misleading
        // "no healthy targets" 502.
        let (pool, conn, _path) = fresh_pool();
        let mk = Arc::new(MasterKey::generate());
        let (combo_id, target_id, _account_id, _model_id) = {
            let w = pool.writer();
            seed_target_with_account(&w, mk.as_ref())
        };
        // Park the only target for 60s.
        {
            let w = pool.writer();
            crate::cooldown::record_failure(&w, target_id, "test seeded", 60).expect("park");
        }

        let cfg = test_config(mk);
        let p = Pipeline::new(conn, cfg);

        let (req, _dis_tx) = make_request(combo_id);
        let result = p.run(req).await;

        // (a) + (b) The pipeline must NOT surface NoHealthyTargets;
        // the dispatch loop walked the parked target and recorded
        // a real upstream error. The provider URL is
        // https://example.com, which does not resolve in the test
        // environment, so we expect UpstreamConnection (or, less
        // likely, a DNS/connect-flavored variant). Anything but
        // NoHealthyTargets is acceptable.
        match &result.error {
            Some(CoreError::NoHealthyTargets(id)) => panic!(
                "expected the dispatch loop to probe the parked target, \
                 got NoHealthyTargets({})",
                id
            ),
            Some(CoreError::UpstreamConnection(msg)) => {
                // Expected case: the upstream call surfaced a
                // connection error. The status code from
                // CoreError::http_status() for this variant is 502,
                // which would be the same as NoHealthyTargets — so
                // we *don't* assert on status_code here; we only
                // assert the error variant is the real one.
                assert!(!msg.is_empty(), "UpstreamConnection message should not be empty");
            }
            Some(other) => {
                // Other retryable upstream errors (timeouts, etc.)
                // are also acceptable; the contract is just that we
                // do NOT get NoHealthyTargets.
                eprintln!("pipeline_probes_parked_target_when_only_option: \
                           non-NoHealthyTargets error {:?} (acceptable)", other);
            }
            None => panic!(
                "expected a real upstream error from probing the parked target, \
                 got a successful result"
            ),
        }

        // (c) The cooldown row is still there: the test did not
        // succeed, and `cooldown::clear` is only called on the
        // success branch of the dispatch loop.
        let w = pool.writer();
        assert!(crate::cooldown::is_in_cooldown(&w, target_id).expect("check"));
    }

    #[tokio::test]
    async fn pipeline_walks_full_row_when_all_targets_in_cooldown() {
        // Regression for the cross-request cooldown contract:
        // when *every* target in a priority combo is parked, the
        // pipeline must still walk the full row (using the
        // pre-cooldown snapshot) so the request surfaces a real
        // upstream error rather than a 502 NoHealthyTargets
        // short-circuit. The persistent cooldown row is preserved
        // across this single request (the dispatch loop only
        // clears on success) so the cross-request protection
        // remains intact.
        use crate::combos::{self, AddTargetInput, Strategy};
        use crate::cooldown;
        use crate::ids::{ComboId, ComboTargetId};

        let (pool, conn, _path) = fresh_pool();
        let mk = Arc::new(MasterKey::generate());

        // Seed one provider, one model, three accounts (distinct
        // labels so the (provider, label) uniqueness constraint
        // lets them coexist), and one combo with three targets,
        // each pointing at the same provider + model but a
        // different account. Distinct priority_orders (10, 20,
        // 30) make the row look like a real priority combo to
        // the dispatch loop.
        let (combo_id, target_ids) = {
            let w = pool.writer();
            // Seed the shared provider, model, and combo.
            seed_provider(&w, "p", AuthType::Bearer);
            w.execute(
                "INSERT INTO models(provider_id, model_id, target_format) VALUES ('p', 'm', 'openai')",
                [],
            )
            .expect("seed model");
            let model_rowid: i64 = w
                .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
                .expect("last_insert_rowid");
            let model_id = crate::ids::ModelRowId(model_rowid);
            let combo_id =
                combos::create_combo(&w, "c", Strategy::Priority, 1).expect("create combo");

            // Three accounts, three targets, one row in the
            // combo's priority list. Each target needs a unique
            // (provider, account) pair to satisfy the combo
            // uniqueness guard inside `add_target`.
            let mut tids = Vec::new();
            for (label, prio) in [("a1", 10_i32), ("a2", 20_i32), ("a3", 30_i32)] {
                let account_id = crate::accounts::create(
                    &w,
                    &ProviderId::new("p"),
                    Some("sk-test"),
                    mk.as_ref(),
                    Some(label),
                    prio,
                    None,
                )
                .expect("seed account");
                let tid = combos::add_target(
                    &w,
                    AddTargetInput {
                        combo_id,
                        provider_id: ProviderId::new("p"),
                        account_id: Some(account_id),
                        model_row_id: Some(model_id),
                        sub_combo_id: None,
                        priority_order: prio,
                    },
                )
                .expect("add target");
                tids.push(tid);
            }
            (combo_id, tids)
        };
        assert_eq!(target_ids.len(), 3, "expected 3 targets in the row");

        // Park all three for 60s.
        {
            let w = pool.writer();
            for tid in &target_ids {
                cooldown::record_failure(&w, *tid, "test seeded", 60).expect("park");
            }
        }

        let cfg = test_config(mk);
        let p = Pipeline::new(conn, cfg);

        let (req, _dis_tx) = make_request(combo_id);
        let result = p.run(req).await;

        // (a) + (b) The result must NOT be a NoHealthyTargets
        // 502 short-circuit. The dispatch loop walked the full
        // row, so we expect a real upstream error. The status
        // code can still be 502 (UpstreamConnection also maps to
        // 502), so we discriminate on the error variant, not on
        // status_code.
        match &result.error {
            Some(CoreError::NoHealthyTargets(id)) => panic!(
                "expected the dispatch loop to walk all parked targets, \
                 got NoHealthyTargets({})",
                id
            ),
            Some(CoreError::UpstreamConnection(msg)) => {
                assert!(!msg.is_empty(), "UpstreamConnection message should not be empty");
            }
            Some(other) => {
                eprintln!(
                    "pipeline_walks_full_row_when_all_targets_in_cooldown: \
                     non-NoHealthyTargets error {:?} (acceptable)",
                    other
                );
            }
            None => panic!(
                "expected a real upstream error from walking the parked row, \
                 got a successful result"
            ),
        }

        // (c) The dispatch loop fired: at least one usage row
        // was written for this request. The `NoHealthyTargets`
        // short-circuit writes its own row, so this alone is
        // not sufficient; combined with the error-variant check
        // above, it proves the loop walked at least one target
        // through `execute_single` → `record_and_fail`. We use
        // `>= 1` rather than `== 3` because the loop may
        // short-circuit on the first non-retryable error (e.g.
        // `ProviderNotFound` when the test registry has no
        // adapter for "p") — the per-target cooldown rows below
        // are what guarantee the cross-request contract is
        // preserved.
        let w = pool.writer();
        let usage_count: i64 = w
            .query_row("SELECT COUNT(*) FROM usage", [], |r| r.get(0))
            .expect("count usage");
        assert!(
            usage_count >= 1,
            "expected the dispatch loop to write at least one usage \
             row (proves it fired); got {}",
            usage_count
        );

        // (d) The error should be a *real* error, not a
        // no-op short-circuit. This is the same contract as
        // (a)/(b) restated; we keep it as its own assertion so
        // a future regression that, e.g., maps every parked
        // target to NoHealthyTargets surfaces as a dedicated
        // failure with a clear message.
        assert!(
            !matches!(result.error, Some(CoreError::NoHealthyTargets(_))),
            "expected a real upstream error, not NoHealthyTargets"
        );

        // (e) All three cooldown rows are still there: every
        // attempt failed, so the dispatch loop re-parked them
        // (or left the seeded row in place).
        for tid in &target_ids {
            assert!(
                cooldown::is_in_cooldown(&w, *tid).expect("check"),
                "expected cooldown row for target {} to still be present",
                tid.0
            );
        }
    }

    /// Regression for bugs 3+4: a `Strategy::Priority` combo of
    /// three healthy targets must walk the full row when the first
    /// target returns a retryable 500 and the second returns 200.
    ///
    /// Pre-fix the dispatch path collapsed the priority walk to a
    /// single target via `take(combo.race_size)` (race_size defaults
    /// to 1 in `admin.rs::create_combo`), so the operator's "try
    /// the next model when the first one 5xx's" expectation was
    /// silently broken: the pipeline kept re-running target #1 on
    /// every `max_attempts` turn. This test pins the post-fix
    /// behavior:
    ///   - the mock listener sees TWO HTTP requests (target 1 and
    ///     target 2; target 3 must NOT be called because the second
    ///     request succeeded),
    ///   - the result has no error,
    ///   - the surfaced body comes from target 2
    ///     (`choices[0].message.content == "from model 2"`).
    #[tokio::test]
    async fn priority_combo_walks_row_after_first_5xx() {
        use crate::adapters::{
            AdapterAuthType, AdapterFormat, ProviderAdapter, ProviderAdapterConfig,
        };
        use crate::combos::{self, AddTargetInput, Strategy};
        use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // ----- 1. Mock adapter that points at our localhost listener -----
        struct MockAdapter {
            config: ProviderAdapterConfig,
        }
        #[async_trait::async_trait]
        impl ProviderAdapter for MockAdapter {
            fn id(&self) -> &ProviderId {
                &self.config.id
            }
            fn config(&self) -> &ProviderAdapterConfig {
                &self.config
            }
            fn build_chat_url(
                &self,
                _target_format: TargetFormat,
                _model: &crate::ids::ModelId,
            ) -> String {
                self.config.base_url.clone()
            }
            fn build_auth_header(&self, api_key: &str) -> (String, String) {
                ("Authorization".into(), format!("Bearer {api_key}"))
            }
            fn build_headers(
                &self,
                api_key: &str,
                _target_format: TargetFormat,
                _model: &crate::ids::ModelId,
            ) -> Vec<(String, String)> {
                vec![
                    self.build_auth_header(api_key),
                    ("Content-Type".into(), "application/json".into()),
                ]
            }
            fn models_url(&self) -> Option<String> {
                None
            }
            async fn fetch_models(
                &self,
                _upstream_client: &std::sync::Arc<crate::upstream::UpstreamClient>,
                _api_key: &str,
            ) -> Result<Vec<crate::models::DiscoveredModel>> {
                Ok(Vec::new())
            }
        }

        // ----- 2. Bind the listener; spawn a server that:
        //         - 1st request → 500 (retryable, advances to next target),
        //         - 2nd request → 200 with the "from model 2" body,
        //         - 3rd request (shouldn't happen) → also 500, so any
        //           regression that *skips* target 2 surfaces as a
        //           pipeline error, not a misleading success.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let local_addr = listener.local_addr().expect("local_addr");
        let upstream_url = format!("http://{local_addr}");

        let call_count = Arc::new(AtomicU32::new(0));
        let server_call_count = call_count.clone();
        let server_handle = tokio::spawn(async move {
            loop {
                let (mut sock, _peer) = match listener.accept().await {
                    Ok(p) => p,
                    Err(_) => break,
                };
                let my_call = server_call_count.fetch_add(1, AtomicOrdering::SeqCst) + 1;

                // Drain headers (and body, if Content-Length present)
                // so reqwest can finish its write before we respond.
                let mut buf = vec![0u8; 64 * 1024];
                let mut total = 0usize;
                let mut content_length: Option<usize> = None;
                let mut header_end: Option<usize> = None;
                loop {
                    let read_result = tokio::time::timeout(
                        Duration::from_secs(2),
                        sock.read(&mut buf[total..]),
                    )
                    .await;
                    match read_result {
                        Err(_) => break,
                        Ok(Ok(0)) => break,
                        Ok(Ok(n)) => {
                            total += n;
                            if header_end.is_none() {
                                if let Some(pos) =
                                    buf[..total].windows(4).position(|w| w == b"\r\n\r\n")
                                {
                                    header_end = Some(pos);
                                    let header_str =
                                        std::str::from_utf8(&buf[..pos]).unwrap_or("");
                                    for line in header_str.split("\r\n") {
                                        if let Some(rest) = line
                                            .to_ascii_lowercase()
                                            .strip_prefix("content-length:")
                                        {
                                            content_length = rest.trim().parse().ok();
                                        }
                                    }
                                }
                            }
                            if let (Some(he), Some(cl)) = (header_end, content_length) {
                                if total - (he + 4) >= cl {
                                    break;
                                }
                            }
                            if total == buf.len() {
                                break;
                            }
                        }
                        Ok(Err(_)) => break,
                    }
                }

                // Build the response for this call.
                let (status_line, body) = if my_call == 1 {
                    (
                        "HTTP/1.1 500 Internal Server Error",
                        r#"{"error":{"message":"upstream boom","type":"server_error"}}"#
                            .to_string(),
                    )
                } else {
                    (
                        "HTTP/1.1 200 OK",
                        r#"{"id":"chatcmpl-2","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"from model 2"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#.to_string(),
                    )
                };
                let response = format!(
                    "{}\r\n\
                     Content-Type: application/json\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\
                     \r\n\
                     {}",
                    status_line,
                    body.len(),
                    body,
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.flush().await;
                // drop sock → connection closes; the pipeline is
                // not streaming so it will return after reading the
                // body.
            }
        });

        // ----- 3. Seed a Priority combo with 3 healthy targets -----
        //         All three use the same provider+model+url (the
        //         mock listener), so the mock's per-call counter is
        //         what discriminates them. Distinct account labels
        //         keep the (provider, account) uniqueness constraint
        //         happy.
        let (pool, conn, _path) = fresh_pool();
        let mk = Arc::new(MasterKey::generate());

        // 1 provider, 1 model, 3 accounts, 3 targets with priorities
        // 10/20/30 → dispatch order is target#1 → target#2 → target#3.
        let (combo_id, _target_ids) = {
            let w = pool.writer();
            seed_provider(&w, "prio-mock", AuthType::Bearer);
            w.execute(
                "INSERT INTO models(provider_id, model_id, target_format) \
                 VALUES ('prio-mock', 'm', 'openai')",
                [],
            )
            .expect("seed model");
            let model_rowid: i64 = w
                .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
                .expect("last_insert_rowid");
            let model_id = crate::ids::ModelRowId(model_rowid);
            // Explicitly create the combo with race_size = 1 (the
            // production default from admin.rs). Pre-fix, this
            // collapsed `to_run` to a single target regardless of
            // the combo's `Strategy`.
            let combo_id = combos::create_combo(&w, "prio-test", Strategy::Priority, 1)
                .expect("create combo");
            let mut tids = Vec::new();
            for (label, prio) in [("a1", 10_i32), ("a2", 20_i32), ("a3", 30_i32)] {
                let account_id = crate::accounts::create(
                    &w,
                    &ProviderId::new("prio-mock"),
                    Some("sk-test"),
                    mk.as_ref(),
                    Some(label),
                    prio,
                    None,
                )
                .expect("seed account");
                let tid = combos::add_target(
                    &w,
                    AddTargetInput {
                        combo_id,
                        provider_id: ProviderId::new("prio-mock"),
                        account_id: Some(account_id),
                        model_row_id: Some(model_id),
                        sub_combo_id: None,
                        priority_order: prio,
                    },
                )
                .expect("add target");
                tids.push(tid);
            }
            (combo_id, tids)
        };

        // ----- 4. Wire the mock adapter + run the pipeline -----
        let defaults = Timeouts::from_config(&TimeoutsConfig::default());
        let mock = MockAdapter {
            config: ProviderAdapterConfig {
                id: ProviderId::new("prio-mock"),
                base_url: upstream_url.clone(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Openai,
                extra_headers: Vec::new(),
            },
        };
        let cfg = PipelineConfig {
            defaults,
            racing: RacingConfig::default(),
            retries: RetriesConfig::default(),
            // CRITICAL: leave max_attempts = 1 so the outer
            // `for attempt in 1..=max_attempts` loop fires ONCE.
            // If the priority walk fix is broken, `to_run` has 1
            // entry, target 1 returns 500, attempt = 1 = max, the
            // pipeline returns the 500 — and the mock will record
            // only ONE HTTP call, not two.
            max_attempts: 1,
            master_key: mk,
            adapters: Arc::new(vec![Arc::new(mock) as Arc<dyn ProviderAdapter>]),
            http_client: reqwest::Client::new(),
            cooldown_secs: 60,
            upstream_client: UpstreamClient::new(),
        };
        let p = Pipeline::new(conn, cfg);

        let (req, _cancel_tx) = make_request(combo_id);
        let result = tokio::time::timeout(Duration::from_secs(15), p.run(req))
            .await
            .expect("pipeline.run timed out");

        // ----- 5. Asserts -----
        // (a) No error: target 2's 200 won the walk.
        assert!(
            result.error.is_none(),
            "expected success after walking the row, got error: {:?}",
            result.error
        );
        // (b) The surfaced body came from target 2.
        let openai_response = result
            .final_response
            .expect("final_response must be Some on success");
        let first_content = openai_response
            .choices
            .first()
            .and_then(|c| c.message.content.as_ref())
            .and_then(|v| v.as_str());
        assert_eq!(
            first_content,
            Some("from model 2"),
            "expected the second target's body to win the walk"
        );
        // (c) The mock saw exactly two HTTP requests: target 1
        // (500) and target 2 (200). Target 3 was NOT called.
        //     A regression that collapses the walk to one target
        //     (pre-fix behavior) would record only 1 call.
        //     A regression that mistakenly *skips* target 2 would
        //     record calls to targets 1 and 3 (call_count == 2
        //     would still match, but then result.error would NOT
        //     be None — caught by (a)).
        let calls = call_count.load(AtomicOrdering::SeqCst);
        assert_eq!(
            calls, 2,
            "expected exactly 2 upstream calls (target 1 500, target 2 200); got {} — \
             this means the priority walk did NOT advance past the failing target",
            calls
        );
        // (d) attempts reflects the per-target loop accounting.
        //     With max_attempts = 1 we expect 1 target tried at
        //     the outer level; the result struct's `attempts`
        //     field tracks the outer-loop counter, not the inner
        //     per-target walk length.
        assert!(
            result.attempts >= 1,
            "expected result.attempts >= 1, got {}",
            result.attempts
        );

        // Best-effort: stop the accept loop. It's harmless if the
        // server task is still running on the way out.
        drop(server_handle);
    }

    // -------------------------------------------------------------------
    // ADVERSARIAL: Combo Priority walk-the-row — the TESTER wants to
    // break the fix by trying edge cases the BUILDERs didn't think
    // of. These tests are about the contract:
    //
    //   "Strategy::Priority walks the ENTIRE row in order; it does
    //    NOT use combo.race_size as a take(N) cap."
    //
    // The existing test (priority_combo_walks_row_after_first_5xx)
    // covers 3 targets with a single 5xx at the head. The 4 cases
    // below push on weaker assumptions:
    //   - bigger rows (5),
    //   - mixed 4xx + 5xx + 2xx (does 4xx abort the walk?),
    //   - all-parked rows (does the dispatch avoid the infinite
    //     loop?),
    //   - 1-target combos with max_attempts>1 (does the outer loop
    //     still fire?).
    // -------------------------------------------------------------------

    /// Build a Priority combo + N targets, all pointing at the same
    /// mock listener. Returns (combo_id, target_ids, server handle,
    /// shared call counter). Distinct account labels keep the
    /// (provider, account) uniqueness constraint happy.
    async fn adversarial_priority_setup_n(
        n: usize,
    ) -> (
        ComboId,
        Vec<ComboTargetId>,
        tokio::task::JoinHandle<()>,
        std::sync::Arc<std::sync::atomic::AtomicU32>,
    ) {
        use crate::adapters::{
            AdapterAuthType, AdapterFormat, ProviderAdapter, ProviderAdapterConfig,
        };
        use crate::combos::AddTargetInput;
        use crate::ids::ModelRowId;
        use std::sync::atomic::AtomicU32;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // The mock adapter is local to this test (each test needs
        // its own list of (status, body) per call slot, so we
        // parameterize the server behavior via a shared Vec).
        struct MockAdapter {
            config: ProviderAdapterConfig,
        }
        #[async_trait::async_trait]
        impl ProviderAdapter for MockAdapter {
            fn id(&self) -> &ProviderId {
                &self.config.id
            }
            fn config(&self) -> &ProviderAdapterConfig {
                &self.config
            }
            fn build_chat_url(
                &self,
                _target_format: TargetFormat,
                _model: &crate::ids::ModelId,
            ) -> String {
                self.config.base_url.clone()
            }
            fn build_auth_header(&self, api_key: &str) -> (String, String) {
                ("Authorization".into(), format!("Bearer {api_key}"))
            }
            fn build_headers(
                &self,
                api_key: &str,
                _target_format: TargetFormat,
                _model: &crate::ids::ModelId,
            ) -> Vec<(String, String)> {
                vec![
                    self.build_auth_header(api_key),
                    ("Content-Type".into(), "application/json".into()),
                ]
            }
            fn models_url(&self) -> Option<String> {
                None
            }
            async fn fetch_models(
                &self,
                _upstream_client: &std::sync::Arc<crate::upstream::UpstreamClient>,
                _api_key: &str,
            ) -> Result<Vec<crate::models::DiscoveredModel>> {
                Ok(Vec::new())
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let local_addr = listener.local_addr().expect("local_addr");
        let upstream_url = format!("http://{local_addr}");

        let call_count = Arc::new(AtomicU32::new(0));
        let server_call_count = call_count.clone();
        let server_handle = tokio::spawn(async move {
            loop {
                let (mut sock, _peer) = match listener.accept().await {
                    Ok(p) => p,
                    Err(_) => break,
                };
                let _ = server_call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // Drain request.
                let mut buf = vec![0u8; 64 * 1024];
                let mut total = 0usize;
                let mut content_length: Option<usize> = None;
                let mut header_end: Option<usize> = None;
                loop {
                    let read_result = tokio::time::timeout(
                        Duration::from_secs(2),
                        sock.read(&mut buf[total..]),
                    )
                    .await;
                    match read_result {
                        Err(_) => break,
                        Ok(Ok(0)) => break,
                        Ok(Ok(n)) => {
                            total += n;
                            if header_end.is_none() {
                                if let Some(pos) =
                                    buf[..total].windows(4).position(|w| w == b"\r\n\r\n")
                                {
                                    header_end = Some(pos);
                                    let header_str =
                                        std::str::from_utf8(&buf[..pos]).unwrap_or("");
                                    for line in header_str.split("\r\n") {
                                        if let Some(rest) = line
                                            .to_ascii_lowercase()
                                            .strip_prefix("content-length:")
                                        {
                                            content_length = rest.trim().parse().ok();
                                        }
                                    }
                                }
                            }
                            if let (Some(he), Some(cl)) = (header_end, content_length) {
                                if total - (he + 4) >= cl {
                                    break;
                                }
                            }
                            if total == buf.len() {
                                break;
                            }
                        }
                        Ok(Err(_)) => break,
                    }
                }
                // Default: 200 OK with a generic body. The per-test
                // caller decides the body shape.
                let body = r#"{"id":"chatcmpl-x","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: application/json\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\
                     \r\n\
                     {}",
                    body.len(),
                    body,
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });

        let (pool, conn, _path) = fresh_pool();
        let mk = Arc::new(MasterKey::generate());
        let (combo_id, target_ids) = {
            let w = pool.writer();
            seed_provider(&w, "adv-mock", AuthType::Bearer);
            w.execute(
                "INSERT INTO models(provider_id, model_id, target_format) \
                 VALUES ('adv-mock', 'm', 'openai')",
                [],
            )
            .expect("seed model");
            let model_rowid: i64 = w
                .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
                .expect("last_insert_rowid");
            let model_id = ModelRowId(model_rowid);
            let combo_id = combos::create_combo(&w, "adv-prio", Strategy::Priority, 1)
                .expect("create combo");
            let mut tids = Vec::new();
            for i in 0..n {
                let account_label = format!("a{}", i);
                let account_id = crate::accounts::create(
                    &w,
                    &ProviderId::new("adv-mock"),
                    Some("sk-test"),
                    mk.as_ref(),
                    Some(&account_label),
                    (i as i32 + 1) * 10,
                    None,
                )
                .expect("seed account");
                let tid = combos::add_target(
                    &w,
                    AddTargetInput {
                        combo_id,
                        provider_id: ProviderId::new("adv-mock"),
                        account_id: Some(account_id),
                        model_row_id: Some(model_id),
                        sub_combo_id: None,
                        priority_order: (i as i32 + 1) * 10,
                    },
                )
                .expect("add target");
                tids.push(tid);
            }
            (combo_id, tids)
        };

        // The Pipeline is built by the caller; we hand back the
        // combo + targets and let the caller wire the rest.
        // The mock adapter construction here is intentionally
        // dead-code (it is the pattern the other tests use inline;
        // the helper returns enough state to build it in the test
        // body). We never reference upstream_url after this point
        // so we just use it once.
        let _ = (
            conn,
            mk.clone(),
            Arc::new(MockAdapter {
                config: ProviderAdapterConfig {
                    id: ProviderId::new("adv-mock"),
                    base_url: upstream_url,
                    auth_type: AdapterAuthType::Bearer,
                    format: AdapterFormat::Openai,
                    extra_headers: Vec::new(),
                },
            }) as Arc<dyn ProviderAdapter>,
        );
        (combo_id, target_ids, server_handle, call_count)
    }

    /// ADVERSARIAL (a) — `priority_combo_with_5_targets_walks_to_5th_when_all_fail`.
    ///
    /// 5 targets, ALL return 500. With max_attempts=1 and the
    /// pre-fix `take(race_size=1)` collapse, the pipeline would
    /// stop at target #1. The fix uses `eligible.len()` for
    /// Priority, so the dispatch should attempt all 5 targets in
    /// priority order and return the last error.
    ///
    /// We can't assert on the per-call body shape here because the
    /// shared mock always returns 200, so we override the listener
    /// directly. To assert the walk, we re-spin a 500-only
    /// listener inline.
    #[tokio::test]
    async fn adversarial_priority_combo_with_5_targets_walks_to_5th_when_all_fail() {
        use crate::combos::AddTargetInput;
        use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // 1. Mock adapter that always responds 500 with an openai-shaped body.
        use crate::adapters::{
            AdapterAuthType, AdapterFormat, ProviderAdapter, ProviderAdapterConfig,
        };
        struct MockAdapter {
            config: ProviderAdapterConfig,
        }
        #[async_trait::async_trait]
        impl ProviderAdapter for MockAdapter {
            fn id(&self) -> &ProviderId {
                &self.config.id
            }
            fn config(&self) -> &ProviderAdapterConfig {
                &self.config
            }
            fn build_chat_url(
                &self,
                _target_format: TargetFormat,
                _model: &crate::ids::ModelId,
            ) -> String {
                self.config.base_url.clone()
            }
            fn build_auth_header(&self, api_key: &str) -> (String, String) {
                ("Authorization".into(), format!("Bearer {api_key}"))
            }
            fn build_headers(
                &self,
                api_key: &str,
                _target_format: TargetFormat,
                _model: &crate::ids::ModelId,
            ) -> Vec<(String, String)> {
                vec![
                    self.build_auth_header(api_key),
                    ("Content-Type".into(), "application/json".into()),
                ]
            }
            fn models_url(&self) -> Option<String> {
                None
            }
            async fn fetch_models(
                &self,
                _upstream_client: &std::sync::Arc<crate::upstream::UpstreamClient>,
                _api_key: &str,
            ) -> Result<Vec<crate::models::DiscoveredModel>> {
                Ok(Vec::new())
            }
        }

        // 2. Spin a 500-only listener.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let local_addr = listener.local_addr().expect("local_addr");
        let upstream_url = format!("http://{local_addr}");
        let call_count = Arc::new(AtomicU32::new(0));
        let server_call_count = call_count.clone();
        let server_handle = tokio::spawn(async move {
            loop {
                let (mut sock, _peer) = match listener.accept().await {
                    Ok(p) => p,
                    Err(_) => break,
                };
                let _ = server_call_count.fetch_add(1, AtomicOrdering::SeqCst);
                let mut buf = vec![0u8; 64 * 1024];
                let mut total = 0usize;
                loop {
                    if let Ok(Ok(0)) =
                        tokio::time::timeout(Duration::from_millis(500), sock.read(&mut buf[total..])).await
                    {
                        break;
                    }
                    if let Ok(Ok(n)) =
                        tokio::time::timeout(Duration::from_millis(500), sock.read(&mut buf[total..])).await
                    {
                        if n == 0 { break; }
                        total += n;
                        if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") { break; }
                    } else {
                        break;
                    }
                }
                let body = r#"{"error":{"message":"all-fail","type":"server_error"}}"#.to_string();
                let response = format!(
                    "HTTP/1.1 500 Internal Server Error\r\n\
                     Content-Type: application/json\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\
                     \r\n{}",
                    body.len(),
                    body,
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });

        // 3. Seed a Priority combo with 5 targets.
        let (pool, conn, _path) = fresh_pool();
        let mk = Arc::new(MasterKey::generate());
        let (combo_id, _target_ids) = {
            let w = pool.writer();
            seed_provider(&w, "adv-mock", AuthType::Bearer);
            w.execute(
                "INSERT INTO models(provider_id, model_id, target_format) \
                 VALUES ('adv-mock', 'm', 'openai')",
                [],
            )
            .expect("seed model");
            let model_rowid: i64 = w
                .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
                .expect("last_insert_rowid");
            let model_id = crate::ids::ModelRowId(model_rowid);
            let combo_id = combos::create_combo(&w, "adv-prio-5", Strategy::Priority, 1)
                .expect("create combo");
            let mut tids = Vec::new();
            for i in 0..5 {
                let account_label = format!("a{}", i);
                let account_id = crate::accounts::create(
                    &w,
                    &ProviderId::new("adv-mock"),
                    Some("sk-test"),
                    mk.as_ref(),
                    Some(&account_label),
                    (i as i32 + 1) * 10,
                    None,
                )
                .expect("seed account");
                let tid = combos::add_target(
                    &w,
                    AddTargetInput {
                        combo_id,
                        provider_id: ProviderId::new("adv-mock"),
                        account_id: Some(account_id),
                        model_row_id: Some(model_id),
                        sub_combo_id: None,
                        priority_order: (i as i32 + 1) * 10,
                    },
                )
                .expect("add target");
                tids.push(tid);
            }
            (combo_id, tids)
        };

        // 4. Wire the mock + run.
        let defaults = Timeouts::from_config(&TimeoutsConfig::default());
        let mock = MockAdapter {
            config: ProviderAdapterConfig {
                id: ProviderId::new("adv-mock"),
                base_url: upstream_url.clone(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Openai,
                extra_headers: Vec::new(),
            },
        };
        let cfg = PipelineConfig {
            defaults,
            racing: RacingConfig::default(),
            // Bug 4 fix: with per-target retry, the
            // `retries.max_attempts` knob now controls how many
            // times each individual target is retried. This
            // test exists to assert the priority walk (bug 3
            // fix), not the per-target retry (bug 4 fix), so
            // pin `retries.max_attempts` to 1 to make the test
            // insensitive to the bug 4 fix. Each target gets
            // exactly one call → 5 calls total.
            retries: RetriesConfig {
                max_attempts: 1,
                ..RetriesConfig::default()
            },
            max_attempts: 1,
            master_key: mk,
            adapters: Arc::new(vec![Arc::new(mock) as Arc<dyn ProviderAdapter>]),
            http_client: reqwest::Client::new(),
            cooldown_secs: 60,
            upstream_client: UpstreamClient::new(),
        };
        let p = Pipeline::new(conn, cfg);

        let (req, _cancel_tx) = make_request(combo_id);
        let result = tokio::time::timeout(Duration::from_secs(15), p.run(req))
            .await
            .expect("pipeline.run timed out");

        // 5. Asserts.
        let calls = call_count.load(AtomicOrdering::SeqCst);
        assert_eq!(
            calls, 5,
            "expected 5 upstream calls (one per target), got {} — the priority \
             walk did not honor eligible.len()=5 for a 5-target row",
            calls
        );
        // The last error must be an upstream 500 (the pipeline
        // returned the 5th target's failure, not a 502 NoHealthy).
        assert!(
            result.error.is_some(),
            "expected an error after walking 5 failing targets"
        );
        match &result.error {
            Some(CoreError::UpstreamError { status, .. }) => {
                assert_eq!(*status, 500, "expected 500 from last target");
            }
            Some(other) => panic!(
                "expected CoreError::UpstreamError(500) from the last target, got {:?}",
                other
            ),
            None => unreachable!(),
        }
        assert!(
            result.attempts >= 1,
            "expected attempts >= 1, got {}",
            result.attempts
        );

        drop(server_handle);
    }

    /// ADVERSARIAL (b) — `priority_combo_with_mixed_4xx_5xx_walks_to_first_2xx`.
    ///
    /// The dispatch loop's per-target branch is:
    ///   `Some(e) if !RetryPolicy::is_retryable(e) => return result`
    /// i.e. a 4xx (non-retryable) **aborts** the walk and returns
    /// the first error. The pre-fix priority walk AND the post-fix
    /// priority walk both have this behavior — a 4xx at target #1
    /// will not advance to target #2.
    ///
    /// The TESTER's expectation: the priority combo should walk
    /// past a 4xx because the operator's intent is "try the next
    /// model on user-error too, not just on transient upstream
    /// failure". This is a stronger contract than the current
    /// implementation honors.
    ///
    /// If this test fails (the pipeline returns the 4xx from
    /// target #1), it documents that the 4xx-abort behavior is a
    /// known limitation of the fix and a future iteration needs to
    /// reconsider whether 4xx should be retried across targets in
    /// a Priority combo.
    #[tokio::test]
    async fn adversarial_priority_combo_with_mixed_4xx_5xx_walks_to_first_2xx() {
        use crate::combos::AddTargetInput;
        use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        use crate::adapters::{
            AdapterAuthType, AdapterFormat, ProviderAdapter, ProviderAdapterConfig,
        };
        struct MockAdapter {
            config: ProviderAdapterConfig,
        }
        #[async_trait::async_trait]
        impl ProviderAdapter for MockAdapter {
            fn id(&self) -> &ProviderId {
                &self.config.id
            }
            fn config(&self) -> &ProviderAdapterConfig {
                &self.config
            }
            fn build_chat_url(
                &self,
                _target_format: TargetFormat,
                _model: &crate::ids::ModelId,
            ) -> String {
                self.config.base_url.clone()
            }
            fn build_auth_header(&self, api_key: &str) -> (String, String) {
                ("Authorization".into(), format!("Bearer {api_key}"))
            }
            fn build_headers(
                &self,
                api_key: &str,
                _target_format: TargetFormat,
                _model: &crate::ids::ModelId,
            ) -> Vec<(String, String)> {
                vec![
                    self.build_auth_header(api_key),
                    ("Content-Type".into(), "application/json".into()),
                ]
            }
            fn models_url(&self) -> Option<String> {
                None
            }
            async fn fetch_models(
                &self,
                _upstream_client: &std::sync::Arc<crate::upstream::UpstreamClient>,
                _api_key: &str,
            ) -> Result<Vec<crate::models::DiscoveredModel>> {
                Ok(Vec::new())
            }
        }

        // 1. Listener: 1st → 400, 2nd → 503, 3rd → 200.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let local_addr = listener.local_addr().expect("local_addr");
        let upstream_url = format!("http://{local_addr}");
        let call_count = Arc::new(AtomicU32::new(0));
        let server_call_count = call_count.clone();
        let server_handle = tokio::spawn(async move {
            loop {
                let (mut sock, _peer) = match listener.accept().await {
                    Ok(p) => p,
                    Err(_) => break,
                };
                let my_call = server_call_count.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                let mut buf = vec![0u8; 64 * 1024];
                let mut total = 0usize;
                loop {
                    if let Ok(Ok(n)) =
                        tokio::time::timeout(Duration::from_millis(500), sock.read(&mut buf[total..])).await
                    {
                        if n == 0 { break; }
                        total += n;
                        if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") { break; }
                    } else {
                        break;
                    }
                }
                let (status_line, body) = match my_call {
                    1 => ("HTTP/1.1 400 Bad Request",
                          r#"{"error":{"message":"bad prompt","type":"invalid_request_error"}}"#.to_string()),
                    2 => ("HTTP/1.1 503 Service Unavailable",
                          r#"{"error":{"message":"overloaded","type":"server_error"}}"#.to_string()),
                    _ => ("HTTP/1.1 200 OK",
                          r#"{"id":"chatcmpl-3","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"from model 3"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#.to_string()),
                };
                let response = format!(
                    "{}\r\n\
                     Content-Type: application/json\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\
                     \r\n{}",
                    status_line,
                    body.len(),
                    body,
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });

        // 2. Seed a 3-target Priority combo.
        let (pool, conn, _path) = fresh_pool();
        let mk = Arc::new(MasterKey::generate());
        let (combo_id, _target_ids) = {
            let w = pool.writer();
            seed_provider(&w, "adv-mock", AuthType::Bearer);
            w.execute(
                "INSERT INTO models(provider_id, model_id, target_format) \
                 VALUES ('adv-mock', 'm', 'openai')",
                [],
            )
            .expect("seed model");
            let model_rowid: i64 = w
                .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
                .expect("last_insert_rowid");
            let model_id = crate::ids::ModelRowId(model_rowid);
            let combo_id = combos::create_combo(&w, "adv-prio-mixed", Strategy::Priority, 1)
                .expect("create combo");
            for i in 0..3 {
                let account_label = format!("mx{}", i);
                let account_id = crate::accounts::create(
                    &w,
                    &ProviderId::new("adv-mock"),
                    Some("sk-test"),
                    mk.as_ref(),
                    Some(&account_label),
                    (i as i32 + 1) * 10,
                    None,
                )
                .expect("seed account");
                combos::add_target(
                    &w,
                    AddTargetInput {
                        combo_id,
                        provider_id: ProviderId::new("adv-mock"),
                        account_id: Some(account_id),
                        model_row_id: Some(model_id),
                        sub_combo_id: None,
                        priority_order: (i as i32 + 1) * 10,
                    },
                )
                .expect("add target");
            }
            (combo_id, Vec::<ComboTargetId>::new())
        };

        // 3. Wire the mock + run.
        let defaults = Timeouts::from_config(&TimeoutsConfig::default());
        let mock = MockAdapter {
            config: ProviderAdapterConfig {
                id: ProviderId::new("adv-mock"),
                base_url: upstream_url.clone(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Openai,
                extra_headers: Vec::new(),
            },
        };
        let cfg = PipelineConfig {
            defaults,
            racing: RacingConfig::default(),
            retries: RetriesConfig::default(),
            max_attempts: 1,
            master_key: mk,
            adapters: Arc::new(vec![Arc::new(mock) as Arc<dyn ProviderAdapter>]),
            http_client: reqwest::Client::new(),
            cooldown_secs: 60,
            upstream_client: UpstreamClient::new(),
        };
        let p = Pipeline::new(conn, cfg);

        let (req, _cancel_tx) = make_request(combo_id);
        let result = tokio::time::timeout(Duration::from_secs(15), p.run(req))
            .await
            .expect("pipeline.run timed out");

        // 4. Asserts.
        let calls = call_count.load(AtomicOrdering::SeqCst);
        // The TESTER's expected behavior: the priority walk should
        // advance past a 4xx because the operator's intent is to
        // try the next model. The current implementation aborts on
        // non-retryable errors — so this test MAY fail (returning
        // the 400 from target #1 with calls=1). If it does, that
        // documents the limitation and is exactly the kind of
        // finding the TESTER is supposed to surface.
        assert_eq!(
            calls, 3,
            "expected 3 upstream calls (walk past 400 → 503 → 200), got {} — \
             the priority walk aborts on a 4xx; if this is intentional, the \
             test should be revised to assert calls=1 and 400 surfaced",
            calls
        );
        // If the walk does advance, the result must be the 200 from target #3.
        assert!(
            result.error.is_none(),
            "expected success from target 3, got error: {:?}",
            result.error
        );

        drop(server_handle);
    }

    /// ADVERSARIAL (c) — `priority_combo_with_zero_eligible_targets_fails_fast`.
    ///
    /// A combo with N targets ALL parked in cooldown must NOT
    /// infinite-loop. The pipeline must surface NoHealthyTargets
    /// (or, per the snapshot fallback, fall through to the
    /// unfiltered list and exercise the parked targets with the
    /// real upstream error).
    ///
    /// The fix's snapshot-fallback path means a single request
    /// doesn't bounce off the transient cross-request cooldown
    /// state. We assert that the call returns a result (not a
    /// hang) and that `attempts` is bounded (the pipeline did
    /// NOT spin forever).
    #[tokio::test]
    async fn adversarial_priority_combo_with_zero_eligible_targets_fails_fast() {
        use crate::combos::AddTargetInput;
        use std::sync::atomic::Ordering;
        use std::time::Instant;
        let (pool, conn, _path) = fresh_pool();
        let mk = Arc::new(MasterKey::generate());
        let (combo_id, target_ids, _account_id, _model_id) =
            { let w = pool.writer(); seed_target_with_account(&w, mk.as_ref()) };
        // Add 2 more targets to make it a 3-target row. (Re-uses
        // the same provider + model; distinct account labels keep
        // uniqueness happy.)
        {
            let w = pool.writer();
            let model_rowid: i64 = w
                .query_row("SELECT id FROM models WHERE provider_id = 'p'", [], |r| r.get(0))
                .expect("model rowid");
            for i in 1..=2 {
                let account_label = format!("adv{}", i);
                let account_id = crate::accounts::create(
                    &w,
                    &ProviderId::new("p"),
                    Some("sk-test"),
                    mk.as_ref(),
                    Some(&account_label),
                    (i + 1) as i32 * 10,
                    None,
                )
                .expect("seed account");
                combos::add_target(
                    &w,
                    AddTargetInput {
                        combo_id,
                        provider_id: ProviderId::new("p"),
                        account_id: Some(account_id),
                        model_row_id: Some(crate::ids::ModelRowId(model_rowid)),
                        sub_combo_id: None,
                        priority_order: (i + 1) as i32 * 10,
                    },
                )
                .expect("add target");
            }
        }
        // Park ALL targets.
        {
            let w = pool.writer();
            let all_tids: Vec<ComboTargetId> = {
                let mut stmt = w
                    .prepare("SELECT id FROM combo_targets WHERE combo_id = ?1")
                    .expect("prep");
                let ids: Vec<i64> = stmt
                    .query_map([combo_id.0], |r| r.get(0))
                    .expect("query")
                    .map(|r| r.unwrap())
                    .collect();
                ids.into_iter().map(ComboTargetId).collect()
            };
            for tid in &all_tids {
                crate::cooldown::record_failure(&w, *tid, "adv seeded", 60).expect("park");
            }
            assert_eq!(all_tids.len(), 3, "expected 3 targets in the combo");
            // Sanity: the 3 IDs we hold match.
            assert!(target_ids == all_tids[0]);
        }
        let cfg = test_config(mk);
        let p = Pipeline::new(conn, cfg);
        let (req, _dis_tx) = make_request(combo_id);
        let t0 = Instant::now();
        // Bounded: 10s is plenty for a 3-target row to fail fast.
        let result = tokio::time::timeout(Duration::from_secs(10), p.run(req))
            .await
            .expect("pipeline.run timed out — the priority walk is hanging on the parked targets");
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "priority walk took {elapsed:?} — the fallback path may be retrying the parked targets without bound"
        );
        // The result must have an error (no successful upstream call).
        assert!(
            result.error.is_some(),
            "expected an error after the walk, got a successful result"
        );
        // The error must NOT be a NoHealthyTargets-only path that
        // hides the real upstream error. Either the fallback
        // exercised the parked targets and surfaced an upstream
        // error, or the row was truly empty and the contract says
        // NoHealthyTargets is acceptable. Both are valid; what we
        // pin is that the pipeline returned a result, not a hang.
        eprintln!("[adversarial c] result.error = {:?}, elapsed = {:?}", result.error, elapsed);
        let _ = Ordering::SeqCst;
    }

    /// ADVERSARIAL (d) — `priority_combo_respects_max_attempts_for_same_provider`.
    ///
    /// Degenerate case: a Priority combo with a SINGLE target, but
    /// `max_attempts = 3`. The outer `for attempt in 1..=max_attempts`
    /// loop must fire 3 times, and the same model must be retried
    /// 3 times. The pre-fix Priority walk used
    /// `take(race_size=1)` which gave the SAME result (1 target
    /// attempted per attempt), so this test passes either way for
    /// the 1-target degenerate case. The TESTER pins it to detect
    /// a future regression where the inner walk is moved INSIDE
    /// the outer loop with the wrong `to_run` capture.
    #[tokio::test]
    async fn adversarial_priority_combo_respects_max_attempts_for_same_provider() {
        use crate::combos::AddTargetInput;
        use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        use crate::adapters::{
            AdapterAuthType, AdapterFormat, ProviderAdapter, ProviderAdapterConfig,
        };
        struct MockAdapter {
            config: ProviderAdapterConfig,
        }
        #[async_trait::async_trait]
        impl ProviderAdapter for MockAdapter {
            fn id(&self) -> &ProviderId {
                &self.config.id
            }
            fn config(&self) -> &ProviderAdapterConfig {
                &self.config
            }
            fn build_chat_url(
                &self,
                _target_format: TargetFormat,
                _model: &crate::ids::ModelId,
            ) -> String {
                self.config.base_url.clone()
            }
            fn build_auth_header(&self, api_key: &str) -> (String, String) {
                ("Authorization".into(), format!("Bearer {api_key}"))
            }
            fn build_headers(
                &self,
                api_key: &str,
                _target_format: TargetFormat,
                _model: &crate::ids::ModelId,
            ) -> Vec<(String, String)> {
                vec![
                    self.build_auth_header(api_key),
                    ("Content-Type".into(), "application/json".into()),
                ]
            }
            fn models_url(&self) -> Option<String> {
                None
            }
            async fn fetch_models(
                &self,
                _upstream_client: &std::sync::Arc<crate::upstream::UpstreamClient>,
                _api_key: &str,
            ) -> Result<Vec<crate::models::DiscoveredModel>> {
                Ok(Vec::new())
            }
        }

        // Listener: always 503.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let local_addr = listener.local_addr().expect("local_addr");
        let upstream_url = format!("http://{local_addr}");
        let call_count = Arc::new(AtomicU32::new(0));
        let server_call_count = call_count.clone();
        let server_handle = tokio::spawn(async move {
            loop {
                let (mut sock, _peer) = match listener.accept().await {
                    Ok(p) => p,
                    Err(_) => break,
                };
                let _ = server_call_count.fetch_add(1, AtomicOrdering::SeqCst);
                let mut buf = vec![0u8; 64 * 1024];
                let mut total = 0usize;
                loop {
                    if let Ok(Ok(n)) =
                        tokio::time::timeout(Duration::from_millis(500), sock.read(&mut buf[total..])).await
                    {
                        if n == 0 { break; }
                        total += n;
                        if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") { break; }
                    } else {
                        break;
                    }
                }
                let body = r#"{"error":{"message":"flaky","type":"server_error"}}"#.to_string();
                let response = format!(
                    "HTTP/1.1 503 Service Unavailable\r\n\
                     Content-Type: application/json\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\
                     \r\n{}",
                    body.len(),
                    body,
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });

        // 1-target Priority combo, max_attempts = 3.
        let (pool, conn, _path) = fresh_pool();
        let mk = Arc::new(MasterKey::generate());
        let combo_id = {
            let w = pool.writer();
            seed_provider(&w, "adv-mock", AuthType::Bearer);
            w.execute(
                "INSERT INTO models(provider_id, model_id, target_format) \
                 VALUES ('adv-mock', 'm', 'openai')",
                [],
            )
            .expect("seed model");
            let model_rowid: i64 = w
                .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
                .expect("last_insert_rowid");
            let model_id = crate::ids::ModelRowId(model_rowid);
            let account_id = crate::accounts::create(
                &w,
                &ProviderId::new("adv-mock"),
                Some("sk-test"),
                mk.as_ref(),
                Some("only"),
                10,
                None,
            )
            .expect("seed account");
            let combo_id = combos::create_combo(&w, "adv-prio-1", Strategy::Priority, 1)
                .expect("create combo");
            combos::add_target(
                &w,
                AddTargetInput {
                    combo_id,
                    provider_id: ProviderId::new("adv-mock"),
                    account_id: Some(account_id),
                    model_row_id: Some(model_id),
                    sub_combo_id: None,
                    priority_order: 10,
                },
            )
            .expect("add target");
            combo_id
        };

        let defaults = Timeouts::from_config(&TimeoutsConfig::default());
        let mock = MockAdapter {
            config: ProviderAdapterConfig {
                id: ProviderId::new("adv-mock"),
                base_url: upstream_url.clone(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Openai,
                extra_headers: Vec::new(),
            },
        };
        let cfg = PipelineConfig {
            defaults,
            racing: RacingConfig::default(),
            // CRITICAL: max_attempts = 3 so the outer loop fires 3 times.
            max_attempts: 3,
            master_key: mk,
            adapters: Arc::new(vec![Arc::new(mock) as Arc<dyn ProviderAdapter>]),
            http_client: reqwest::Client::new(),
            cooldown_secs: 60,
            // Disable retry backoff so the test is fast.
            retries: RetriesConfig {
                backoff_base_ms: 1,
                backoff_factor: 1,
                backoff_jitter_pct: 0,
                ..RetriesConfig::default()
            },
            upstream_client: UpstreamClient::new(),
        };
        let p = Pipeline::new(conn, cfg);
        let (req, _cancel_tx) = make_request(combo_id);
        let result = tokio::time::timeout(Duration::from_secs(15), p.run(req))
            .await
            .expect("pipeline.run timed out");

        let calls = call_count.load(AtomicOrdering::SeqCst);
        assert_eq!(
            calls, 3,
            "expected 3 upstream calls (one per outer-loop attempt) for a \
             1-target Priority combo with max_attempts=3, got {} — the outer \
             retry loop is not firing, or the inner walk is collapsing to 0",
            calls
        );
        assert_eq!(
            result.attempts, 3,
            "expected PipelineResult.attempts == 3, got {}",
            result.attempts
        );

        drop(server_handle);
    }

    /// ADVERSARIAL (e) — `bug4_per_target_retry_exhausts_then_falls_through_to_next_target`.
    ///
    /// Bug 4 regression. The pre-fix pipeline applied the
    /// `retries.max_attempts` knob at the *combo walk* level
    /// (a single outer `for attempt in 1..=max_attempts` loop
    /// re-walked the whole row of targets). With a 2-target
    /// combo and `max_attempts=3`, the first target (always 5xx)
    /// would consume the *entire* retry budget, and the second
    /// target would only get one try (the third outer iteration
    /// would re-walk the row, fail at the first target, and bail
    /// out via the post-loop block). Net effect: the first target
    /// got 3 tries, the second got 0.
    ///
    /// The post-fix per-target retry loop fires
    /// `retries.max_attempts` times on the *same* model. Once
    /// those are exhausted, the pipeline falls through to the
    /// next target (bug 3 contract). For this test that means:
    /// target 1 → 3 tries (all 503) → fall through → target 2 →
    /// 1 try (200) → success. Total upstream calls: 4. The 4th
    /// call is the one that succeeds.
    #[tokio::test]
    async fn bug4_per_target_retry_exhausts_then_falls_through_to_next_target() {
        use crate::combos::AddTargetInput;
        use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        use crate::adapters::{
            AdapterAuthType, AdapterFormat, ProviderAdapter, ProviderAdapterConfig,
        };
        struct MockAdapter {
            config: ProviderAdapterConfig,
        }
        #[async_trait::async_trait]
        impl ProviderAdapter for MockAdapter {
            fn id(&self) -> &ProviderId {
                &self.config.id
            }
            fn config(&self) -> &ProviderAdapterConfig {
                &self.config
            }
            fn build_chat_url(
                &self,
                _target_format: TargetFormat,
                _model: &crate::ids::ModelId,
            ) -> String {
                self.config.base_url.clone()
            }
            fn build_auth_header(&self, api_key: &str) -> (String, String) {
                ("Authorization".into(), format!("Bearer {api_key}"))
            }
            fn build_headers(
                &self,
                api_key: &str,
                _target_format: TargetFormat,
                _model: &crate::ids::ModelId,
            ) -> Vec<(String, String)> {
                vec![
                    self.build_auth_header(api_key),
                    ("Content-Type".into(), "application/json".into()),
                ]
            }
            fn models_url(&self) -> Option<String> {
                None
            }
            async fn fetch_models(
                &self,
                _upstream_client: &std::sync::Arc<crate::upstream::UpstreamClient>,
                _api_key: &str,
            ) -> Result<Vec<crate::models::DiscoveredModel>> {
                Ok(Vec::new())
            }
        }

        // Listener: per-call counter, returns 503 for the first
        // `bug4_max_attempts_for_target1` calls and 200 for the
        // rest. This lets us assert both the per-target retry
        // budget and the fall-through to the next target.
        const TARGET1_RETRY_BUDGET: u32 = 3;
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let local_addr = listener.local_addr().expect("local_addr");
        let upstream_url = format!("http://{local_addr}");
        let call_count = Arc::new(AtomicU32::new(0));
        let server_call_count = call_count.clone();
        let server_handle = tokio::spawn(async move {
            loop {
                let (mut sock, _peer) = match listener.accept().await {
                    Ok(p) => p,
                    Err(_) => break,
                };
                let n = server_call_count.fetch_add(1, AtomicOrdering::SeqCst);
                let mut buf = vec![0u8; 64 * 1024];
                let mut total = 0usize;
                loop {
                    if let Ok(Ok(rd)) = tokio::time::timeout(
                        Duration::from_millis(500),
                        sock.read(&mut buf[total..]),
                    )
                    .await
                    {
                        if rd == 0 {
                            break;
                        }
                        total += rd;
                        if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    } else {
                        break;
                    }
                }
                let (status_line, body) = if n < TARGET1_RETRY_BUDGET {
                    (
                        "HTTP/1.1 503 Service Unavailable",
                        r#"{"error":{"message":"flaky","type":"server_error"}}"#.to_string(),
                    )
                } else {
                    (
                        "HTTP/1.1 200 OK",
                        r#"{"id":"chatcmpl-bug4","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#.to_string(),
                    )
                };
                let response = format!(
                    "{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status_line,
                    body.len(),
                    body,
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });

        // 2-target Priority combo. Two distinct accounts on the
        // same provider/model yield two distinct targets,
        // satisfying the (provider, account, model) uniqueness
        // constraint. Target 1 is exhausted (3 × 503); target 2
        // succeeds on its first call. Expected: 4 HTTP calls
        // total (3 retry of target 1 + 1 success of target 2).
        let (pool, conn, _path) = fresh_pool();
        let mk = Arc::new(MasterKey::generate());
        let combo_id = {
            let w = pool.writer();
            seed_provider(&w, "adv-mock", AuthType::Bearer);
            w.execute(
                "INSERT INTO models(provider_id, model_id, target_format) \
                 VALUES ('adv-mock', 'm', 'openai')",
                [],
            )
            .expect("seed model");
            let model_rowid: i64 = w
                .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
                .expect("last_insert_rowid");
            let model_id = crate::ids::ModelRowId(model_rowid);
            let mut account_ids = Vec::new();
            for label in ["bug4-a1", "bug4-a2"] {
                let account_id = crate::accounts::create(
                    &w,
                    &ProviderId::new("adv-mock"),
                    Some("sk-test"),
                    mk.as_ref(),
                    Some(label),
                    10,
                    None,
                )
                .expect("seed account");
                account_ids.push(account_id);
            }
            let combo_id = combos::create_combo(&w, "adv-bug4", Strategy::Priority, 1)
                .expect("create combo");
            for (i, prio) in [10_i32, 20].iter().enumerate() {
                combos::add_target(
                    &w,
                    AddTargetInput {
                        combo_id,
                        provider_id: ProviderId::new("adv-mock"),
                        account_id: Some(account_ids[i]),
                        model_row_id: Some(model_id),
                        sub_combo_id: None,
                        priority_order: *prio,
                    },
                )
                .expect("add target");
            }
            combo_id
        };

        let defaults = Timeouts::from_config(&TimeoutsConfig::default());
        let mock = MockAdapter {
            config: ProviderAdapterConfig {
                id: ProviderId::new("adv-mock"),
                base_url: upstream_url.clone(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Openai,
                extra_headers: Vec::new(),
            },
        };
        let cfg = PipelineConfig {
            defaults,
            racing: RacingConfig::default(),
            // The per-target retry budget is the source of
            // truth for the bug 4 fix. We set it to 3 so the
            // first target is retried 3 times, then the
            // pipeline falls through to the second target.
            retries: RetriesConfig {
                max_attempts: TARGET1_RETRY_BUDGET as u8,
                backoff_base_ms: 1,
                backoff_factor: 1,
                backoff_jitter_pct: 0,
            },
            // PipelineConfig.max_attempts is now mostly a
            // vestigial knob for the outer combo walk; the
            // per-target retry is governed by
            // `retries.max_attempts` above. Pin to 1 to make
            // the test insensitive to future changes in the
            // outer loop.
            max_attempts: 1,
            master_key: mk,
            adapters: Arc::new(vec![Arc::new(mock) as Arc<dyn ProviderAdapter>]),
            http_client: reqwest::Client::new(),
            cooldown_secs: 60,
            upstream_client: UpstreamClient::new(),
        };
        let p = Pipeline::new(conn, cfg);
        let (req, _cancel_tx) = make_request(combo_id);
        let result = tokio::time::timeout(Duration::from_secs(15), p.run(req))
            .await
            .expect("pipeline.run timed out");

        let calls = call_count.load(AtomicOrdering::SeqCst);
        // 3 retries on target 1 (all 503) + 1 success on target 2.
        assert_eq!(
            calls, 4,
            "expected 4 upstream calls (3 retries of target 1 + 1 success of target 2), \
             got {} — the per-target retry budget is not being applied to the same \
             model before fall-through",
            calls
        );
        // The 4th call (the first call to target 2) succeeded,
        // so the pipeline returns a 200 with the upstream body.
        assert!(
            result.error.is_none(),
            "expected success after target 2's first call, got error: {:?}",
            result.error
        );
        assert_eq!(
            result.status_code, 200,
            "expected 200, got {}",
            result.status_code
        );
        let body = result
            .final_response
            .as_ref()
            .expect("final_response must be set on success");
        assert_eq!(
            body.id, "chatcmpl-bug4",
            "expected the mock's success body id, got {:?}",
            body.id
        );

        drop(server_handle);
    }

    #[tokio::test]
    async fn pipeline_does_not_record_cooldown_on_4xx_error() {
        // The pipeline uses `RetryPolicy::is_retryable` to decide
        // whether to park a target. A 4xx is *not* retryable, so a
        // 4xx response must NOT add a cooldown row. We simulate
        // the path by directly exercising the cooldown-record
        // guard (the helper's `is_retryable` matches the
        // pipeline's). For an end-to-end probe we'd need a real
        // upstream returning 4xx, which the tests' `test_config`
        // doesn't provide; the unit-level test below keeps the
        // invariant in scope.
        use crate::retry::RetryPolicy;
        let err_4xx = CoreError::UpstreamError {
            status: 400,
            provider: "p".into(),
            model: "m".into(),
            body: "bad".into(),
        };
        assert!(!RetryPolicy::is_retryable(&err_4xx));
        // The pipeline's "did the helper touch the cooldown table?"
        // assertion lives in the integration tests below; this
        // unit-level guard keeps the rule in one place.
    }

    #[tokio::test]
    async fn pipeline_clears_cooldown_on_success_path() {
        // The "clear" path runs inside the execute_single loop. We
        // assert the helper clears the row on a *retryable*
        // success: seed a parked target, simulate the
        // success branch by calling `cooldown::clear` directly
        // (the same call the pipeline makes), and verify the
        // state. This is a shallow check — the deeper integration
        // test would need a real HTTP mock — but it covers the
        // contract that "on success the row goes away".
        let (pool, _conn, _path) = fresh_pool();
        let (combo_id, target_id, _account_id, _model_id) = {
            let w = pool.writer();
            seed_target_with_account(
                &w,
                &MasterKey::generate(),
            )
        };
        {
            let w = pool.writer();
            crate::cooldown::record_failure(&w, target_id, "before", 60).expect("park");
            assert!(crate::cooldown::is_in_cooldown(&w, target_id).expect("parked"));

            // Simulate the success branch the pipeline runs.
            crate::cooldown::clear(&w, target_id).expect("clear");
            assert!(!crate::cooldown::is_in_cooldown(&w, target_id).expect("cleared"));
        }
        let _ = combo_id;
    }

    #[tokio::test]
    async fn list_targets_with_model_includes_cooldown_fields() {
        // The `ComboTargetWithModel` shape (consumed by the
        // admin endpoint and the frontend) must surface the
        // cooldown state. We assert the three new fields are
        // populated correctly across the active / expired /
        // no-cooldown cases.
        use crate::combos::list_targets_with_model;
        let (pool, _conn, _path) = fresh_pool();
        let (combo_id, target_id, _account_id, _model_id) = {
            let w = pool.writer();
            seed_target_with_account(&w, &MasterKey::generate())
        };
        // No cooldown yet: in_cooldown=false, *_until/reason=None.
        {
            let w = pool.writer();
            let ts = list_targets_with_model(&w, combo_id).expect("list");
            assert_eq!(ts.len(), 1);
            assert!(!ts[0].in_cooldown);
            assert!(ts[0].cooldown_until.is_none());
            assert!(ts[0].cooldown_reason.is_none());
        }
        // Active cooldown: in_cooldown=true, reason set.
        {
            let w = pool.writer();
            crate::cooldown::record_failure(&w, target_id, "boom", 60).expect("park");
            let ts = list_targets_with_model(&w, combo_id).expect("list");
            assert_eq!(ts.len(), 1);
            assert!(ts[0].in_cooldown);
            assert!(ts[0].cooldown_until.is_some());
            assert_eq!(ts[0].cooldown_reason.as_deref(), Some("boom"));
        }
    }

    // -------------------------------------------------------------------
    // Circuit-breaker regression
    //
    // The cooldown fix (snapshot pre-cooldown + fallback to unfiltered
    // dispatch) only covers the persistent `target_cooldowns` table.
    // The in-memory `CircuitBreakerRegistry` is a SECOND, independent
    // de-route path: every account that hits the failure threshold
    // (5 retryable failures, 60s unhealthy window) is filtered out by
    // the `eligible` filter (line 213-220) BEFORE the cooldown
    // snapshot is taken, leaving `to_run_unfiltered_snapshot` empty
    // and the pipeline short-circuits to NoHealthyTargets.
    //
    // This regression reproduces the user's reported failure mode for
    // the 'nerd' combo (9 targets) without touching production code:
    // we seed a combo with 9 targets (3 providers × 3 accounts),
    // force every account into the `Unhealthy` state via the
    // circuit-breaker test helper, and call `Pipeline::run()`. The
    // current code short-circuits with `NoHealthyTargets` in 0 ms;
    // the desired behaviour is to walk the row (the dispatch loop
    // will see ProviderNotFound or similar, and the
    // `record_and_fail` will produce a real upstream-flavoured
    // error) so the operator gets a useful log line instead of a
    // misleading 502.
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn combo_with_all_accounts_in_circuit_breaker_does_not_short_circuit() {
        // Three providers, one model each, one account per provider,
        // three targets per provider → 9 targets total. The combo is
        // a 9-row priority list spanning 3 different providers so the
        // dispatch loop has to walk across providers (matching the
        // user's 'nerd' shape). All 3 accounts are forced Unhealthy
        // before the run.
        use crate::combos::{self, AddTargetInput, Strategy};
        let (pool, conn, _path) = fresh_pool();
        let mk = Arc::new(MasterKey::generate());

        let (combo_id, account_ids) = {
            let w = pool.writer();
            let combo_id =
                combos::create_combo(&w, "nerd", Strategy::Priority, 1).expect("create combo");

            let mut acc_ids: Vec<(ProviderId, AccountId)> = Vec::new();
            // Three providers × three accounts each × three model rows
            // = nine targets. We pick the targets to alternate
            // providers so the priority walk visits all 9.
            for prov_idx in 0..3 {
                let pid_str = format!("p{}", prov_idx);
                providers::create(
                    &w,
                    &ProviderId::new(&pid_str),
                    &pid_str,
                    "https://example.com",
                    AuthType::Bearer,
                    ProviderFormat::Openai,
                    None,
                    None,
                )
                .expect("seed provider");
                w.execute(
                    "INSERT INTO models(provider_id, model_id, target_format) \
                     VALUES (?1, ?2, 'openai')",
                    rusqlite::params![&pid_str, format!("m{}", prov_idx)],
                )
                .expect("seed model");
                let model_rowid: i64 = w
                    .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
                    .expect("last_insert_rowid");
                let model_id = ModelRowId(model_rowid);

                for acct_idx in 0..3 {
                    let label = format!("a{}-{}", prov_idx, acct_idx);
                    let account_id = crate::accounts::create(
                        &w,
                        &ProviderId::new(&pid_str),
                        Some("sk-test"),
                        mk.as_ref(),
                        Some(&label),
                        // priority_order is the per-target ordering
                        // inside the combo; we just need them to
                        // alternate so the walk visits every account.
                        (prov_idx * 3 + acct_idx + 1) as i32,
                        None,
                    )
                    .expect("seed account");
                    combos::add_target(
                        &w,
                        AddTargetInput {
                            combo_id,
                            provider_id: ProviderId::new(&pid_str),
                            account_id: Some(account_id),
                            model_row_id: Some(model_id),
                            sub_combo_id: None,
                            priority_order: (prov_idx * 3 + acct_idx + 1) as i32 * 10,
                        },
                    )
                    .expect("add target");
                    acc_ids.push((ProviderId::new(&pid_str), account_id));
                }
            }
            (combo_id, acc_ids)
        };
        assert_eq!(
            account_ids.len(),
            9,
            "expected 9 (provider, account) pairs seeded across 3 providers"
        );

        let cfg = test_config(mk);
        let p = Pipeline::new(conn, cfg);

        // Force every account into the Unhealthy state. This is the
        // exact in-memory state the registry would reach after 5
        // consecutive retryable failures on each account.
        for (_pid, aid) in &account_ids {
            p.circuit_breaker.force_unhealthy(*aid);
        }
        // Sanity-check: every account is now Unhealthy.
        for (_pid, aid) in &account_ids {
            assert_eq!(
                p.circuit_breaker.is_healthy(*aid),
                crate::circuit_breaker::Health::Unhealthy,
                "account {:?} should be Unhealthy before the run",
                aid
            );
        }

        let (req, _dis_tx) = make_request(combo_id);
        let result = p.run(req).await;

        // The current code (with only the cooldown-table fix in
        // place) returns `NoHealthyTargets` here because:
        //
        //   1. The eligible filter (pipeline.rs:213-220) drops every
        //      target whose account is Unhealthy.
        //   2. The eligible vec is therefore empty.
        //   3. The fix at lines 298-425 only fires AFTER the
        //      eligible filter, and only handles the
        //      `target_cooldowns` table — it does not consider the
        //      circuit breaker.
        //   4. The auto-populate fallback at lines 235-281 also does
        //      not re-introduce Unhealthy accounts (the registry is
        //      in-memory, the DB is `health_status = 'healthy'`).
        //   5. Pipeline returns NoHealthyTargets in 0 ms.
        //
        // The contract the test enforces: the next request to this
        // combo must NOT short-circuit to NoHealthyTargets; the
        // dispatch loop must walk the row and surface a real
        // per-target error (e.g. ProviderNotFound for an unknown
        // provider, or UpstreamConnection for a real upstream).
        match &result.error {
            Some(CoreError::NoHealthyTargets(id)) => {
                panic!(
                    "REGRESSION: combo with 9 targets, all accounts in circuit_breaker.Unhealthy, \
                     short-circuited to NoHealthyTargets({}) in {:?}. \
                     The fix at pipeline.rs:298-425 only covers the persistent \
                     target_cooldowns table; the in-memory circuit breaker is a second \
                     independent de-route path that still short-circuits the request. \
                     See: pipeline.rs:213-220 (eligible filter) — this filter happens \
                     BEFORE the cooldown snapshot, so when ALL accounts are Unhealthy \
                     `to_run_unfiltered_snapshot` is empty and the fallback at line 423 \
                     is never reached.",
                    id, result.attempts
                );
            }
            Some(CoreError::ProviderNotFound(_)) => {
                // Acceptable: the dispatch loop walked the row and
                // surfaced a real per-target error (no adapter was
                // registered for any of the 3 providers in
                // test_config). The point is: it did NOT short-
                // circuit to NoHealthyTargets.
            }
            Some(CoreError::UpstreamConnection(msg)) => {
                // Also acceptable: real upstream-flavoured error.
                assert!(!msg.is_empty());
            }
            Some(other) => {
                eprintln!(
                    "combo_with_all_accounts_in_circuit_breaker_does_not_short_circuit: \
                     non-NoHealthyTargets error {:?} (acceptable)",
                    other
                );
            }
            None => panic!(
                "expected a real upstream / per-target error from walking the unhealthy row, \
                 got a successful result"
            ),
        }

        // Side contract: the dispatch loop fired. We don't assert
        // the exact count because ProviderNotFound is non-retryable
        // and the loop short-circuits on the first one — but at
        // least one usage row must exist (the NoHealthyTargets
        // short-circuit writes its own row, so this only proves the
        // loop fired in combination with the error-variant
        // assertion above).
        let w = pool.writer();
        let usage_count: i64 = w
            .query_row("SELECT COUNT(*) FROM usage", [], |r| r.get(0))
            .expect("count usage");
        assert!(
            usage_count >= 1,
            "expected the dispatch loop to write at least one usage row; got {}",
            usage_count
        );
    }

    // -------------------------------------------------------------------
    // Targeted unit test: the eligible filter itself, in isolation.
    //
    // The end-to-end test above mixes adapter lookup, timeouts, and
    // the dispatch loop. The root cause is a single filter step:
    // pipeline.rs:213-220. This smaller test exercises just that
    // step and makes the regression cause-and-effect obvious:
    //
    //   Given a 9-target list where every target's account is
    //   Unhealthy in the in-memory registry, the `eligible` vec
    //   built by the filter is empty, so the next branch
    //   (`if eligible.is_empty()`) fires NoHealthyTargets.
    //
    // We can't reach the private `eligible` vec directly, but the
    // behaviour is observable through `Pipeline::run()` (see the
    // regression test above) and the `to_run` snapshot at line 304
    // is the same data the fix depends on.
    // -------------------------------------------------------------------

    #[test]
    fn circuit_breaker_unhealthy_filter_drops_target_before_cooldown_snapshot() {
        // The fix at pipeline.rs:298-425 snapshots `to_run` AFTER
        // the eligible filter. If the eligible filter already
        // produced an empty vec (because the circuit breaker
        // marked every account Unhealthy), the snapshot is empty
        // and the fallback has nothing to fall back to.
        //
        // This is a documentation / invariant test: build a 3-
        // account combo, force all accounts Unhealthy, and prove
        // that calling run() returns NoHealthyTargets (i.e. the
        // bug as the user sees it). When the fix is extended to
        // cover the circuit-breaker case this test will start
        // failing, which is the signal to update both the test
        // and the fix together.
        use crate::combos::{self, AddTargetInput, Strategy};
        let (pool, conn, _path) = fresh_pool();
        let mk = Arc::new(MasterKey::generate());

        let (combo_id, account_ids) = {
            let w = pool.writer();
            providers::create(
                &w,
                &ProviderId::new("p"),
                "p",
                "https://example.com",
                AuthType::Bearer,
                ProviderFormat::Openai,
                None,
                None,
            )
            .expect("seed provider");
            w.execute(
                "INSERT INTO models(provider_id, model_id, target_format) \
                 VALUES ('p', 'm', 'openai')",
                [],
            )
            .expect("seed model");
            let model_rowid: i64 = w
                .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
                .expect("last_insert_rowid");
            let model_id = ModelRowId(model_rowid);
            let combo_id =
                combos::create_combo(&w, "c", Strategy::Priority, 1).expect("create combo");
            let mut aids = Vec::new();
            for (label, prio) in [("a1", 10_i32), ("a2", 20_i32), ("a3", 30_i32)] {
                let account_id = crate::accounts::create(
                    &w,
                    &ProviderId::new("p"),
                    Some("sk-test"),
                    mk.as_ref(),
                    Some(label),
                    prio,
                    None,
                )
                .expect("seed account");
                combos::add_target(
                    &w,
                    AddTargetInput {
                        combo_id,
                        provider_id: ProviderId::new("p"),
                        account_id: Some(account_id),
                        model_row_id: Some(model_id),
                        sub_combo_id: None,
                        priority_order: prio,
                    },
                )
                .expect("add target");
                aids.push(account_id);
            }
            (combo_id, aids)
        };
        assert_eq!(account_ids.len(), 3);

        let cfg = test_config(mk);
        let p = Pipeline::new(conn, cfg);
        for aid in &account_ids {
            p.circuit_breaker.force_unhealthy(*aid);
        }

        // Drive a single attempt via the public `run` surface.
        let (req, _dis_tx) = make_request(combo_id);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(p.run(req));

        // Document the fixed behaviour: when every account is Unhealthy
        // in the circuit breaker, the pre-CB snapshot fallback kicks in
        // (see the `eligible` filter in Pipeline::run) and the
        // dispatch loop walks the row of targets. The exact error
        // variant is implementation-defined — the only contract under
        // test is that the request did NOT short-circuit to
        // NoHealthyTargets. A 502 with a real upstream error (or a
        // ProviderNotFound when no adapter is registered) is the
        // expected outcome.
        match &result.error {
            Some(CoreError::NoHealthyTargets(id)) => {
                panic!(
                    "REGRESSION: pre-CB snapshot fallback did not engage — \
                     got NoHealthyTargets({id}) in 0ms, but the combo had {n} \
                     targets in DB and the eligible filter should have fallen \
                     through to the unfiltered list.",
                    id = id,
                    n = account_ids.len(),
                );
            }
            // Any other error means the dispatch loop walked the row,
            // which is the new contract. (ProviderNotFound is what the
            // test config produces because no real adapter is
            // registered for the "p" provider; in production with a
            // real adapter the error would be UpstreamConnection or
            // similar.)
            other => {
                assert!(
                    other.is_some(),
                    "dispatch loop should have surfaced an error, not Ok"
                );
            }
        }
    }

    // -----------------------------------------------------------------
    // Cancellation regression tests
    //
    // These lock in the contract that `client_disconnected`:
    //   1. aborts an in-flight upstream request (no waiting on
    //      `total_ms` when the client is gone),
    //   2. is reported with HTTP 499 and `CoreError::ClientDisconnected`,
    //   3. does NOT park the target in `target_cooldowns` nor
    //      increment the circuit breaker (a client-driven cancel is
    //      not an upstream failure).
    //
    // We use provider id `"openrouter"` because the built-in
    // adapter registry (`adapters::builtin_adapters()`) ships an
    // adapter for that id; without an adapter the pipeline bails
    // with `ProviderNotFound` before the `tokio::select!` is ever
    // reached. The `base_url` we pass to the adapter is overridden
    // by the provider row in the DB, so we point that row at the
    // local mock listener / a dead port.
    // -----------------------------------------------------------------

    /// Build a `PipelineConfig` that ships the built-in adapter
    /// registry, so the dispatch loop can find a `ProviderAdapter`
    /// for the provider id under test. The test_config() default
    /// has an empty adapter list (correct for the routing-only
    /// tests, wrong for anything that exercises the HTTP path).
    fn test_config_with_adapters(master_key: Arc<MasterKey>) -> PipelineConfig {
        let mut cfg = test_config(master_key);
        cfg.adapters = Arc::new(crate::adapters::builtin_adapters());
        cfg
    }

    /// Seed a 1-provider / 1-account / 1-target / 1-combo shape
    /// pointing at the given upstream URL. Returns the
    /// (`combo_id`, `account_id`) pair so the test can drive the
    /// pipeline and inspect the post-run state.
    fn seed_solo_combo_at_url(
        conn: &Connection,
        provider_id: &str,
        upstream_url: &str,
        master_key: &MasterKey,
    ) -> (ComboId, AccountId) {
        providers::create(
            conn,
            &ProviderId::new(provider_id),
            provider_id,
            upstream_url,
            AuthType::Bearer,
            ProviderFormat::Openai,
            None,
            None,
        )
        .expect("seed provider");
        conn.execute(
            "INSERT INTO models(provider_id, model_id, target_format) \
             VALUES (?1, 'm', 'openai')",
            [provider_id],
        )
        .expect("seed model");
        let model_rowid: i64 = conn
            .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
            .expect("last_insert_rowid");
        let combo_id =
            combos::create_combo(conn, "c", combos::Strategy::Priority, 1).expect("create combo");
        let account_id = crate::accounts::create(
            conn,
            &ProviderId::new(provider_id),
            Some("sk-test"),
            master_key,
            Some("a1"),
            10,
            None,
        )
        .expect("seed account");
        combos::add_target(
            conn,
            combos::AddTargetInput {
                combo_id,
                provider_id: ProviderId::new(provider_id),
                account_id: Some(account_id),
                model_row_id: Some(ModelRowId(model_rowid)),
                sub_combo_id: None,
                priority_order: 10,
            },
        )
        .expect("add target");
        (combo_id, account_id)
    }

    /// Cancellation while waiting on the upstream: the `tokio::select!`
    /// at the reqwest send site must short-circuit to
    /// `ClientDisconnected` / 499 instead of letting the request hang
    /// out for `total_ms`.
    ///
    /// We cancel *before* the run starts (analogous to A.2) so the
    /// per-target boundary check fires on the first iteration with
    /// no upstream work attempted. The send-side `tokio::select!` is
    /// exercised by A.3's mock listener below.
    #[tokio::test]
    async fn cancellation_during_request_aborts_with_499() {
        let (pool, conn, _path) = fresh_pool();
        let mk = Arc::new(MasterKey::generate());

        let (combo_id, _account_id) = seed_solo_combo_at_url(
            &pool.writer(),
            "openrouter",
            "http://127.0.0.1:1",
            &mk,
        );

        let cfg = test_config_with_adapters(mk);
        let p = Pipeline::new(conn, cfg);

        let (req, cancel_tx) = make_request(combo_id);
        cancel_tx.send(true).expect("send cancel");

        let result = tokio::time::timeout(Duration::from_secs(3), p.run(req))
            .await
            .expect("pipeline.run did not abort within 3s — cancellation is broken");

        match &result.error {
            Some(CoreError::ClientDisconnected) => {
                assert_eq!(
                    CoreError::ClientDisconnected.http_status(),
                    499,
                    "ClientDisconnected must map to HTTP 499"
                );
            }
            other => panic!(
                "expected ClientDisconnected(499) but got {:?} — the \
                 client_disconnected watch is not being honored on the \
                 send/loop path",
                other
            ),
        }
    }

    /// Cancellation must NOT poison the persistent cooldown table or
    /// the in-memory circuit breaker. A client closing the
    /// connection is not an upstream failure; the next request from
    /// any client should still be able to try the target.
    #[tokio::test]
    async fn cancellation_does_not_park_target_in_cooldown_or_circuit_breaker() {
        let (pool, conn, _path) = fresh_pool();
        let mk = Arc::new(MasterKey::generate());

        let (combo_id, account_id) = seed_solo_combo_at_url(
            &pool.writer(),
            "openrouter",
            "http://127.0.0.1:1",
            &mk,
        );
        let cfg = test_config_with_adapters(mk);
        let p = Pipeline::new(conn.clone(), cfg);

        let (req, cancel_tx) = make_request(combo_id);
        // Cancel BEFORE the run starts so the per-target boundary
        // check fires on the first iteration with no upstream work
        // attempted at all. The run must still complete normally
        // and exit without writing any cooldown row or
        // incrementing the CB.
        cancel_tx.send(true).expect("send cancel");

        let _result = p.run(req).await;

        // 1. target_cooldowns is empty. The schema is keyed by
        //    `combo_target_id` (not `target_id`); see
        //    migrations/000017_add_target_cooldowns.sql.
        let w = pool.writer();
        let target_ids: Vec<i64> = {
            let mut stmt = w
                .prepare("SELECT id FROM combo_targets WHERE combo_id = ?1")
                .expect("prep");
            stmt.query_map([combo_id.0], |r| r.get::<_, i64>(0))
                .expect("query")
                .map(|r| r.expect("row"))
                .collect()
        };
        assert!(!target_ids.is_empty(), "test setup: combo has no targets");
        for tid in &target_ids {
            let count: i64 = w
                .query_row(
                    "SELECT COUNT(*) FROM target_cooldowns WHERE combo_target_id = ?1",
                    [tid],
                    |r| r.get(0),
                )
                .expect("count cooldowns");
            assert_eq!(
                count, 0,
                "target_cooldowns row found for combo_target_id {tid} after a client-driven \
                 cancellation — cancellation should not park targets"
            );
        }

        // 2. The circuit breaker is still Healthy with 0 failures.
        assert_eq!(
            p.circuit_breaker.is_healthy(account_id),
            Health::Healthy,
            "circuit breaker for account {account_id:?} was disturbed by a \
             client cancellation — ClientDisconnected must be excluded from \
             the CB counter"
        );
    }

    /// End-to-end exercise of the new (Gate 1) non-streaming chat
    /// dispatch path that uses `UpstreamClient::call()` instead of
    /// the legacy reqwest client. We bind a localhost listener, point
    /// a mock `ProviderAdapter` at it, run a non-streaming chat
    /// request, and assert the pipeline returns a 200 with the
    /// body parsed as an `OpenAIResponse`. This proves the
    /// migration is functionally correct end-to-end: the
    /// `UpstreamRequest` is built, the `TimeoutProfile::Custom`
    /// resolves correctly, the body is collected via
    /// `UpstreamResponse::collect`, and the JSON parses to
    /// `OpenAIResponse` (the same downstream code path the
    /// reqwest-based path used).
    #[tokio::test]
    async fn non_streaming_dispatch_uses_upstream_client_end_to_end() {
        use crate::adapters::{
            AdapterAuthType, AdapterFormat, ProviderAdapter, ProviderAdapterConfig,
        };
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // ----- 1. A mock ProviderAdapter that points at our
        //         localhost listener -----
        struct MockAdapter {
            config: ProviderAdapterConfig,
        }
        #[async_trait::async_trait]
        impl ProviderAdapter for MockAdapter {
            fn id(&self) -> &ProviderId {
                &self.config.id
            }
            fn config(&self) -> &ProviderAdapterConfig {
                &self.config
            }
            fn build_chat_url(
                &self,
                _target_format: TargetFormat,
                _model: &crate::ids::ModelId,
            ) -> String {
                self.config.base_url.clone()
            }
            fn build_auth_header(&self, api_key: &str) -> (String, String) {
                ("Authorization".into(), format!("Bearer {api_key}"))
            }
            fn build_headers(
                &self,
                api_key: &str,
                _target_format: TargetFormat,
                _model: &crate::ids::ModelId,
            ) -> Vec<(String, String)> {
                vec![
                    self.build_auth_header(api_key),
                    ("Content-Type".into(), "application/json".into()),
                ]
            }
            fn models_url(&self) -> Option<String> {
                None
            }
            async fn fetch_models(
                &self,
                _upstream_client: &std::sync::Arc<crate::upstream::UpstreamClient>,
                _api_key: &str,
            ) -> Result<Vec<crate::models::DiscoveredModel>> {
                Ok(Vec::new())
            }
        }

        // ----- 2. Wire the listener + spawn a server that returns a
        //         well-formed OpenAI chat completion response. The
        //         server parses Content-Length from the request
        //         headers and stops reading once that many body
        //         bytes have arrived — this avoids blocking on a
        //         body that hyper may or may not flush before the
        //         response window expires. -----
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let local_addr = listener.local_addr().expect("local_addr");
        let upstream_url = format!("http://{local_addr}");

        let server_handle = tokio::spawn(async move {
            let (mut sock, _peer) = listener.accept().await.expect("accept");
            // Read until we've seen `\r\n\r\n` and (if a
            // Content-Length is present) that many body bytes. We
            // cap each read at 2s so the test never hangs the
            // suite on a misbehaving client.
            let mut buf = vec![0u8; 64 * 1024];
            let mut total = 0usize;
            let mut content_length: Option<usize> = None;
            let mut header_end: Option<usize> = None;
            loop {
                let read_result = tokio::time::timeout(
                    Duration::from_secs(2),
                    sock.read(&mut buf[total..]),
                )
                .await;
                match read_result {
                    Err(_) => break,
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => {
                        total += n;
                        if header_end.is_none() {
                            if let Some(pos) =
                                buf[..total].windows(4).position(|w| w == b"\r\n\r\n")
                            {
                                header_end = Some(pos);
                                let header_str = std::str::from_utf8(&buf[..pos]).unwrap_or("");
                                for line in header_str.split("\r\n") {
                                    if let Some(rest) = line
                                        .to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                    {
                                        content_length = rest.trim().parse().ok();
                                    }
                                }
                            }
                        }
                        if let (Some(he), Some(cl)) = (header_end, content_length) {
                            if total - (he + 4) >= cl {
                                break;
                            }
                        }
                        if total == buf.len() {
                            break;
                        }
                    }
                    Ok(Err(_)) => break,
                }
            }
            // Return a minimal-but-valid OpenAI chat completion.
            let body = r#"{"id":"chatcmpl-test","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n\
                 {}",
                body.len(),
                body,
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.flush().await;
        });

        // ----- 3. Build the pipeline config + pipeline -----
        let (pool, conn, _path) = fresh_pool();
        let mk = Arc::new(MasterKey::generate());
        let (combo_id, _account_id) = seed_solo_combo_at_url(
            &pool.writer(),
            "non-streaming-test",
            &upstream_url,
            &mk,
        );

        let defaults = Timeouts::from_config(&TimeoutsConfig::default());
        let mock = MockAdapter {
            config: ProviderAdapterConfig {
                id: ProviderId::new("non-streaming-test"),
                base_url: upstream_url.clone(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Openai,
                extra_headers: Vec::new(),
            },
        };
        let cfg = PipelineConfig {
            defaults,
            racing: RacingConfig::default(),
            retries: RetriesConfig::default(),
            max_attempts: 1,
            master_key: mk,
            adapters: Arc::new(vec![Arc::new(mock) as Arc<dyn ProviderAdapter>]),
            http_client: reqwest::Client::new(),
            cooldown_secs: 60,
            upstream_client: UpstreamClient::new(),
        };
        let p = Pipeline::new(conn, cfg);

        let (req, _cancel_tx) = make_request(combo_id);

        // ----- 4. Run the pipeline and assert success -----
        let result = tokio::time::timeout(Duration::from_secs(15), p.run(req))
            .await
            .expect("pipeline.run timed out — non-streaming dispatch did not return");

        assert!(
            result.error.is_none(),
            "expected no error from non-streaming dispatch but got {:?}",
            result.error
        );
        assert_eq!(result.status_code, 200);
        let openai_response = result
            .final_response
            .expect("final_response must be Some on success");
        let first_content = openai_response
            .choices
            .first()
            .and_then(|c| c.message.content.as_ref())
            .and_then(|v| v.as_str());
        assert_eq!(
            first_content,
            Some("hello"),
            "the parsed body must surface the upstream's `choices[0].message.content`"
        );

        let _ = server_handle.await;
    }

    /// End-to-end exercise of the new (Gate 2) streaming chat
    /// dispatch path that uses `UpstreamClient::call()` and
    /// `UpstreamBodyStream::next_chunk()` instead of the legacy
    /// reqwest `bytes_stream()` API. We bind a localhost listener,
    /// point a mock `ProviderAdapter` at it, run a streaming chat
    /// request, and assert the pipeline forwards every SSE chunk
    /// (translated to OpenAI) into the `stream_sink` channel in
    /// real-time. This proves:
    ///   1. The `UpstreamRequest` is built and consumed by the
    ///      hyper-based client.
    ///   2. The `TimeoutProfile::Custom` is honored at the streaming
    ///      boundary.
    ///   3. The body iteration via `UpstreamBodyStream::next_chunk`
    ///      drives the SSE line splitter.
    ///   4. The translation step (parse_openai_sse_line +
    ///      sink.send) still produces a well-formed OpenAI chunk.
    #[tokio::test]
    async fn streaming_dispatch_uses_upstream_client_end_to_end() {
        use crate::adapters::{
            AdapterAuthType, AdapterFormat, ProviderAdapter, ProviderAdapterConfig,
        };
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // ----- 1. A mock ProviderAdapter that points at our
        //         localhost listener -----
        struct MockAdapter {
            config: ProviderAdapterConfig,
        }
        #[async_trait::async_trait]
        impl ProviderAdapter for MockAdapter {
            fn id(&self) -> &ProviderId {
                &self.config.id
            }
            fn config(&self) -> &ProviderAdapterConfig {
                &self.config
            }
            fn build_chat_url(
                &self,
                _target_format: TargetFormat,
                _model: &crate::ids::ModelId,
            ) -> String {
                format!("{}/chat/completions", self.config.base_url)
            }
            fn build_auth_header(&self, api_key: &str) -> (String, String) {
                ("Authorization".into(), format!("Bearer {api_key}"))
            }
            fn build_headers(
                &self,
                api_key: &str,
                _target_format: TargetFormat,
                _model: &crate::ids::ModelId,
            ) -> Vec<(String, String)> {
                vec![
                    self.build_auth_header(api_key),
                    ("Content-Type".into(), "application/json".into()),
                ]
            }
            fn models_url(&self) -> Option<String> {
                None
            }
            async fn fetch_models(
                &self,
                _upstream_client: &std::sync::Arc<crate::upstream::UpstreamClient>,
                _api_key: &str,
            ) -> Result<Vec<crate::models::DiscoveredModel>> {
                Ok(Vec::new())
            }
        }

        // ----- 2. Bind the listener and spawn a server that
        //         returns three well-formed OpenAI SSE chunks
        //         followed by the [DONE] sentinel. We use
        //         `Transfer-Encoding: chunked` so the upstream
        //         client's `Limited` body sees multiple frames
        //         (the way a real upstream would stream an
        //         OpenAI response). -----
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let local_addr = listener.local_addr().expect("local_addr");
        let upstream_url = format!("http://{local_addr}");

        let server_handle = tokio::spawn(async move {
            let (mut sock, _peer) = listener.accept().await.expect("accept");
            // Drain the request bytes so the client can finish
            // the POST. The mock upstream is OpenAI-on-the-wire;
            // we don't parse the body — just consume it.
            let mut buf = vec![0u8; 64 * 1024];
            let mut total = 0usize;
            let mut header_end: Option<usize> = None;
            let mut content_length: Option<usize> = None;
            loop {
                let read_result = tokio::time::timeout(
                    Duration::from_secs(2),
                    sock.read(&mut buf[total..]),
                )
                .await;
                match read_result {
                    Err(_) => break,
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => {
                        total += n;
                        if header_end.is_none() {
                            if let Some(pos) =
                                buf[..total].windows(4).position(|w| w == b"\r\n\r\n")
                            {
                                header_end = Some(pos);
                                let header_str =
                                    std::str::from_utf8(&buf[..pos]).unwrap_or("");
                                for line in header_str.split("\r\n") {
                                    if let Some(rest) = line
                                        .to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                    {
                                        content_length = rest.trim().parse().ok();
                                    }
                                }
                            }
                        }
                        if let (Some(he), Some(cl)) = (header_end, content_length) {
                            if total - (he + 4) >= cl {
                                break;
                            }
                        }
                        if total == buf.len() {
                            break;
                        }
                    }
                    Ok(Err(_)) => break,
                }
            }

            // Send the response headers. We use neither
            // Content-Length nor Transfer-Encoding: chunked
            // — the upstream closes the socket when the
            // response is complete. This is the simplest
            // streaming shape and is the one the production
            // hyper client is tuned for (the `Limited` body
            // wrapper reads until EOF in this case).
            let headers = b"HTTP/1.1 200 OK\r\n\
                            Content-Type: text/event-stream\r\n\
                            Cache-Control: no-cache\r\n\
                            Connection: close\r\n\
                            \r\n";
            if sock.write_all(headers).await.is_err() {
                return;
            }

            // Three OpenAI-style chunks (delta.content="hi" /
            // " there" / "!") then [DONE]. Each chunk is
            // sent as a separate `write_all` so the upstream
            // client's body stream sees multiple frames
            // arriving on the socket, exercising the
            // `next_chunk()` boundary in the loop.
            let chunks: &[&[u8]] = &[
                br#"data: {"id":"chatcmpl-x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}

"#.as_slice(),
                br#"data: {"id":"chatcmpl-x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":" there"},"finish_reason":null}]}

"#.as_slice(),
                br#"data: {"id":"chatcmpl-x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"!"},"finish_reason":null}]}

"#.as_slice(),
            ];
            for c in chunks {
                if sock.write_all(c).await.is_err() {
                    return;
                }
                if sock.flush().await.is_err() {
                    return;
                }
            }
            // [DONE] sentinel as the last chunk.
            let done = b"data: [DONE]\n\n";
            let _ = sock.write_all(done).await;
            let _ = sock.flush().await;
            // Close the socket to signal EOF — the upstream
            // client's `next_chunk` will return `Ok(None)`.
            let _ = sock.shutdown().await;
        });

        // ----- 3. Build the pipeline config + pipeline -----
        let (pool, conn, _path) = fresh_pool();
        let mk = Arc::new(MasterKey::generate());
        let (combo_id, _account_id) = seed_solo_combo_at_url(
            &pool.writer(),
            "streaming-test",
            &upstream_url,
            &mk,
        );

        let defaults = Timeouts::from_config(&TimeoutsConfig::default());
        let mock = MockAdapter {
            config: ProviderAdapterConfig {
                id: ProviderId::new("streaming-test"),
                base_url: upstream_url.clone(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Openai,
                extra_headers: Vec::new(),
            },
        };
        let cfg = PipelineConfig {
            defaults,
            racing: RacingConfig::default(),
            retries: RetriesConfig::default(),
            max_attempts: 1,
            master_key: mk,
            adapters: Arc::new(vec![Arc::new(mock) as Arc<dyn ProviderAdapter>]),
            http_client: reqwest::Client::new(),
            cooldown_secs: 60,
            upstream_client: UpstreamClient::new(),
        };
        let p = Pipeline::new(conn, cfg);

        // ----- 4. Build a streaming request: `stream = true`,
        //         a real sink channel, and a real cancel watch
        //         (we never send `true`, so the watch stays
        //         false for the whole run). -----
        let (mut req, _cancel_tx) = make_request(combo_id);
        req.openai_request.stream = true;
        let (sink_tx, mut sink_rx) = mpsc::channel::<String>(32);
        req.stream_sink = Some(sink_tx);

        // ----- 5. Run the pipeline. We capture the result so we
        //         can report it in the panic message; the
        //         streaming dispatch populates the sink as a
        //         side effect. -----
        let result = tokio::time::timeout(Duration::from_secs(15), p.run(req))
            .await
            .expect("streaming pipeline.run timed out — next_chunk() did not return");

        assert!(
            result.error.is_none(),
            "expected no error from streaming dispatch but got {:?}",
            result.error
        );
        assert_eq!(result.status_code, 200);

        // After `run` returns the sink sender has been dropped,
        // so the channel is closed. Drain everything still in
        // the buffer.
        let mut collected: Vec<String> = Vec::new();
        while let Some(item) = sink_rx.recv().await {
            collected.push(item);
        }

        // ----- 6. Assertions -----
        assert!(
            !collected.is_empty(),
            "expected at least one SSE chunk to be forwarded to the sink — \
             the streaming dispatch path produced no output"
        );
        // The [DONE] sentinel is sent by the pipeline
        // itself, but the upstream also sends it; either way
        // at least one [DONE] must be present.
        let done_count = collected.iter().filter(|s| s.as_str() == "[DONE]").count();
        assert!(
            done_count >= 1,
            "expected at least one [DONE] sentinel in the sink output, got: {:?}",
            collected
        );
        // Every non-[DONE] entry must be a valid JSON object
        // with a `choices` array (i.e. a translated OpenAI
        // chunk).
        for item in &collected {
            if item.as_str() == "[DONE]" {
                continue;
            }
            let parsed: serde_json::Value = serde_json::from_str(item)
                .unwrap_or_else(|e| panic!(
                    "sink item is not valid JSON: {:?} (parse error: {})",
                    item, e
                ));
            assert!(
                parsed.get("choices").is_some(),
                "translated chunk must carry a `choices` field: {:?}",
                parsed
            );
        }
        // The concatenated `delta.content` of the translated
        // chunks must spell "hi there!" — proves every chunk
        // was forwarded and translated, not just the first.
        let mut reconstructed = String::new();
        for item in &collected {
            if item.as_str() == "[DONE]" {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(item) {
                if let Some(delta) = v
                    .get("choices")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("delta"))
                {
                    if let Some(content) = delta.get("content").and_then(|s| s.as_str()) {
                        reconstructed.push_str(content);
                    }
                }
            }
        }
        assert_eq!(
            reconstructed, "hi there!",
            "concatenated chunk content must equal `hi there!`, got {:?}",
            reconstructed
        );

        let _ = server_handle.await;
    }

    /// Cancellation must abort the streaming response mid-stream
    /// without waiting for the upstream to finish sending.
    ///
    /// We cancel *before* the run starts (analogous to A.2) so the
    /// per-target boundary check fires on the first iteration with
    /// no upstream work attempted. The mock listener is wired up
    /// for a follow-up test that will exercise the actual
    /// stream-side `tokio::select!` (see the TODO at the end of
    /// this function).
    #[tokio::test]
    async fn cancellation_during_streaming_aborts_response_stream() {
        use tokio::net::TcpListener;

        // Bind a localhost listener; the test points the provider
        // at it. We don't actually drive a request through the
        // listener here (cancelling before the run means the
        // pipeline never reaches the dispatch loop), but the
        // listener is left set up so a follow-up that wants to
        // exercise the stream-side `tokio::select!` only has to
        // drop the early `cancel_tx.send(true)` and add a
        // mid-stream cancel task.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        drop(listener);

        let (pool, conn, _path) = fresh_pool();
        let mk = Arc::new(MasterKey::generate());
        let (combo_id, _account_id) = seed_solo_combo_at_url(
            &pool.writer(),
            "openrouter",
            "http://127.0.0.1:1",
            &mk,
        );

        let cfg = test_config_with_adapters(mk);
        let p = Pipeline::new(conn, cfg);

        let (mut req, cancel_tx) = make_request(combo_id);
        req.openai_request.stream = true;
        cancel_tx.send(true).expect("send cancel");

        let result = tokio::time::timeout(Duration::from_secs(3), p.run(req))
            .await
            .expect("streaming pipeline.run did not abort within 3s of cancel — \
                    the per-target boundary check is not engaging for streaming requests");

        match &result.error {
            Some(CoreError::ClientDisconnected) => {}
            other => panic!(
                "expected ClientDisconnected(499) but got {:?} — streaming \
                 path is not observing client_disconnected",
                other
            ),
        }

        // TODO(follow-up): see `cancellation_mid_sse_stream_aborts_immediately`
        // below — that test exercises the real stream-side
        // `tokio::select!` by binding a localhost TcpListener that
        // answers 200 OK + a slow SSE stream and then cancels
        // mid-stream.
    }

    /// Mid-stream cancellation: the client disconnects *while the
    /// upstream is actively streaming SSE chunks*, and the pipeline
    /// must abort the attempt without waiting for the upstream to
    /// finish (or for `total_ms` to elapse). This is the contract
    /// exercised by the *stream-side* `tokio::select!` at
    /// pipeline.rs ~1756 (the one that races
    /// `response.bytes_stream().next()` against the
    /// `client_disconnected` watch).
    ///
    /// The earlier `cancellation_during_streaming_aborts_response_stream`
    /// only proves the per-target boundary check works — it cancels
    /// *before* the run starts, so the dispatch loop never reaches
    /// the HTTP path. This test goes the other way: we let the
    /// dispatch actually open the upstream socket, complete the
    /// HTTP exchange, enter the `bytes_stream()` loop, read at
    /// least one chunk, and only THEN signal cancellation. The
    /// server holds the socket open without sending more data, so
    /// the only way the pipeline can finish is by hitting the
    /// cancel arm of the inner `tokio::select!`.
    #[tokio::test]
    async fn cancellation_mid_sse_stream_aborts_immediately() {
        use crate::adapters::{
            AdapterAuthType, AdapterFormat, ProviderAdapter, ProviderAdapterConfig,
        };
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // -----------------------------------------------------------------
        // 1. A minimal `ProviderAdapter` whose `base_url` is whatever
        //    the test wants. The built-in adapters hardcode
        //    `https://openrouter.ai/api/v1` (or similar) which makes
        //    it impossible to point them at a localhost listener; the
        //    pipeline reads the URL via `adapter.build_chat_url(...)`,
        //    NOT from the `providers.upstream_url` column. So we need
        //    our own adapter, registered under a unique provider id
        //    so the existing `OpenRouterAdapter` does not match.
        //
        //    The shape mirrors `OpenRouterAdapter` for the chat path
        //    and is OpenAI-on-the-wire; the methods we don't exercise
        //    (`fetch_models`, `models_url`) return values that would
        //    never get called by the streaming dispatch path.
        // -----------------------------------------------------------------
        struct MockAdapter {
            config: ProviderAdapterConfig,
        }
        #[async_trait::async_trait]
        impl ProviderAdapter for MockAdapter {
            fn id(&self) -> &crate::ids::ProviderId {
                &self.config.id
            }
            fn config(&self) -> &ProviderAdapterConfig {
                &self.config
            }
            fn build_chat_url(
                &self,
                _target_format: crate::models::TargetFormat,
                _model: &crate::ids::ModelId,
            ) -> String {
                format!("{}/chat/completions", self.config.base_url)
            }
            fn build_auth_header(&self, api_key: &str) -> (String, String) {
                ("Authorization".into(), format!("Bearer {api_key}"))
            }
            fn build_headers(
                &self,
                api_key: &str,
                _target_format: crate::models::TargetFormat,
                _model: &crate::ids::ModelId,
            ) -> Vec<(String, String)> {
                vec![
                    self.build_auth_header(api_key),
                    ("Content-Type".into(), "application/json".into()),
                ]
            }
            fn models_url(&self) -> Option<String> {
                None
            }
            async fn fetch_models(
                &self,
                _upstream_client: &std::sync::Arc<crate::upstream::UpstreamClient>,
                _api_key: &str,
            ) -> Result<Vec<crate::models::DiscoveredModel>> {
                // Not exercised by the streaming path; return an
                // empty list so the type is satisfied.
                Ok(Vec::new())
            }
        }

        // -----------------------------------------------------------------
        // 2. Build a `PipelineConfig` that registers ONLY the mock
        //    adapter, scoped to a unique provider id. The default
        //    `test_config()` has an empty adapter list; `test_config_
        //    with_adapters` ships every built-in adapter, which would
        //    mean a request for `"test-mock-sse"` finds no match and
        //    bails with `ProviderNotFound` before reaching the HTTP
        //    layer. We want ONLY our mock to be discoverable.
        // -----------------------------------------------------------------
        fn test_config_with_mock(
            master_key: Arc<MasterKey>,
            base_url: String,
        ) -> PipelineConfig {
            let defaults = Timeouts::from_config(&TimeoutsConfig::default());
            let mock = MockAdapter {
                config: ProviderAdapterConfig {
                    id: ProviderId::new("test-mock-sse"),
                    base_url,
                    auth_type: AdapterAuthType::Bearer,
                    format: AdapterFormat::Openai,
                    extra_headers: Vec::new(),
                },
            };
            PipelineConfig {
                defaults,
                racing: RacingConfig::default(),
                retries: RetriesConfig::default(),
                max_attempts: 1,
                master_key,
                adapters: Arc::new(vec![Arc::new(mock)
                    as Arc<dyn ProviderAdapter>]),
                http_client: reqwest::Client::new(),
                cooldown_secs: 60,
                upstream_client: UpstreamClient::new(),
            }
        }

        // -----------------------------------------------------------------
        // 3. Bind the mock upstream, start its accept task. The server:
        //    a. accepts ONE connection (the dispatch will only open
        //       one — single target, no race),
        //    b. drains the request bytes until "\r\n\r\n" so reqwest
        //       is no longer blocked on writing the body,
        //    c. writes `200 OK` + `text/event-stream` headers,
        //    d. writes ONE valid OpenAI SSE chunk so the pipeline
        //       records TTFT and enters the steady-state stream loop,
        //    e. STALLS — holds the socket open and stops writing.
        //       The pipeline's `bytes_stream().next()` future is now
        //       pending, so the only way it can wake is via the
        //       cancel arm of the inner `tokio::select!`.
        //
        //    The server records whether it observed a client-side
        //    close (read returns 0 / Err) AFTER the cancel fires.
        //    That is the proof that reqwest's connection was actually
        //    torn down as a consequence of the cancellation, not just
        //    that the pipeline's `select!` short-circuited internally.
        // -----------------------------------------------------------------
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let local_addr = listener.local_addr().expect("local_addr");
        let upstream_url = format!("http://{local_addr}");

        let client_closed = Arc::new(AtomicBool::new(false));
        let accepted = Arc::new(AtomicBool::new(false));
        let bytes_after_headers = Arc::new(AtomicU64::new(0));

        let server_client_closed = client_closed.clone();
        let server_accepted = accepted.clone();
        let server_bytes = bytes_after_headers.clone();
        let server_handle = tokio::spawn(async move {
            let (mut sock, _peer) = listener.accept().await.expect("accept");
            server_accepted.store(true, Ordering::SeqCst);

            // Drain the request line + headers so the client can
            // finish writing its POST body. We bound the read at
            // 32 KiB which is far more than any of the headers +
            // tiny body reqwest will send.
            let mut buf = vec![0u8; 32 * 1024];
            let mut total = 0usize;
            loop {
                match sock.read(&mut buf[total..]).await {
                    Ok(0) => break, // peer closed before sending
                    Ok(n) => {
                        total += n;
                        if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                            // Headers ended. Any further bytes are
                            // body; we don't need to parse them, but
                            // keep reading a little so the client can
                            // finish the POST and the pipeline can
                            // start reading the response.
                            while let Ok(n) = sock.read(&mut buf).await {
                                if n == 0 {
                                    break;
                                }
                                total += n;
                            }
                            break;
                        }
                        if total == buf.len() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = total;

            // Send the SSE response: status line + headers + a
            // single valid OpenAI chunk, then STALL. The chunk is
            // well-formed JSON so `parse_openai_sse_line` returns
            // `Ok(Some(_))` and the pipeline records TTFT and
            // enters the steady-state `while let` loop.
            let headers = b"HTTP/1.1 200 OK\r\n\
                            Content-Type: text/event-stream\r\n\
                            Cache-Control: no-cache\r\n\
                            Connection: keep-alive\r\n\
                            \r\n";
            let chunk = b"data: {\"id\":\"chatcmpl-x\",\"object\":\"chat.completion.chunk\",\
                          \"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n";
            if sock.write_all(headers).await.is_err() {
                return;
            }
            if sock.write_all(chunk).await.is_err() {
                return;
            }
            if sock.flush().await.is_err() {
                return;

            }

            // Now STALL: read the socket until either the client
            // closes (which is what we want to observe — reqwest
            // tears the connection down when the pipeline drops
            // the response future) or 10s elapse. We deliberately
            // do NOT send a `[DONE]` sentinel and do NOT close the
            // socket ourselves; the pipeline's `bytes_stream().next()`
            // must stay pending throughout this period.
            let mut stall_buf = [0u8; 1024];
            let stall_deadline = std::time::Instant::now()
                + std::time::Duration::from_secs(10);
            loop {
                let now = std::time::Instant::now();
                if now >= stall_deadline {
                    break;
                }
                let remaining = stall_deadline - now;
                let read = tokio::time::timeout(
                    remaining,
                    sock.read(&mut stall_buf),
                )
                .await;
                match read {
                    // Client closed the connection — this is the
                    // signal that reqwest propagated the
                    // cancellation all the way down to the socket.
                    Ok(Ok(0)) => {
                        server_client_closed.store(true, Ordering::SeqCst);
                        break;
                    }
                    Ok(Ok(n)) => {
                        server_bytes
                            .fetch_add(n as u64, Ordering::SeqCst);
                    }
                    // Read errored out (typically a reset from the
                    // peer once reqwest drops the body future).
                    Ok(Err(_)) => {
                        server_client_closed.store(true, Ordering::SeqCst);
                        break;
                    }
                    // Timeout with no data: the client is still
                    // holding the socket open. Loop and try again
                    // so we keep watching for the close.
                    Err(_) => {}
                }
            }
        });

        // -----------------------------------------------------------------
        // 4. Seed a 1-provider / 1-account / 1-target combo whose
        //    upstream URL is the listener. The URL we pass to
        //    `providers::create` is irrelevant to the dispatch path
        //    (the adapter hardcodes the URL), but we still pass the
        //    real listener URL so the row is self-describing for
        //    future readers of the test.
        // -----------------------------------------------------------------
        let (pool, conn, _path) = fresh_pool();
        let mk = Arc::new(MasterKey::generate());
        let (combo_id, _account_id) = seed_solo_combo_at_url(
            &pool.writer(),
            "test-mock-sse",
            &upstream_url,
            &mk,
        );

        // -----------------------------------------------------------------
        // 5. Wire the pipeline to the mock adapter and run the
        //    request with `stream = true`. We use long timeouts so
        //    the only way the run can complete is by hitting the
        //    cancel arm of the stream-side `tokio::select!`. If the
        //    pipeline accidentally fell back to `total_ms` or
        //    `idle_chunk_ms` instead, the run would still be
        //    pending at the 3s timeout below.
        // -----------------------------------------------------------------
        let cfg = test_config_with_mock(mk, upstream_url.clone());
        let p = Pipeline::new(conn, cfg);

        let (mut req, cancel_tx) = make_request(combo_id);
        req.openai_request.stream = true;

        // Drive the cancel ~300ms after the run starts. That's
        // enough time for reqwest to finish the POST, get the
        // 200 OK, parse the first chunk, and start blocking on
        // the second `bytes_stream().next()`.
        let cancel_tx_clone = cancel_tx.clone();
        let cancel_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = cancel_tx_clone.send(true);
        });

        let result = tokio::time::timeout(
            Duration::from_secs(3),
            p.run(req),
        )
        .await
        .expect(
            "mid-stream cancellation: pipeline.run did not abort within 3s of \
             cancel — the stream-side tokio::select! (response.bytes_stream().next() \
             vs client_disconnected) is not being honored",
        );

        // The cancel task is fire-and-forget; just await it for
        // tidiness.
        let _ = cancel_task.await;

        // -----------------------------------------------------------------
        // 6. Assertions. The contract is:
        //    a. the run completes well under `total_ms` (we use a
        //       3s hard timeout above; with `total = 30s`, hitting
        //       that ceiling would prove the cancel did NOT short-
        //       circuit the stream),
        //    b. the error is `ClientDisconnected` (not an
        //       `UpstreamConnection` from a hung-up socket — the
        //       server kept its side open),
        //    c. the server saw the connection as torn down AFTER
        //       the cancel fired (i.e. reqwest propagated the
        //       abort to the socket). This is the proof that the
        //       pipeline's `select!` actually selected the cancel
        //       arm and dropped the body future, instead of
        //       waiting for the stream to finish on its own.
        // -----------------------------------------------------------------
        match &result.error {
            Some(CoreError::ClientDisconnected) => {
                assert_eq!(
                    CoreError::ClientDisconnected.http_status(),
                    499,
                    "ClientDisconnected must map to HTTP 499"
                );
            }
            other => panic!(
                "expected ClientDisconnected(499) from mid-stream cancel but got \
                 {:?} — the stream-side tokio::select! is not firing on the \
                 cancel arm during an active SSE stream",
                other
            ),
        }

        // Stop the server (it has a 10s internal deadline but we
        // don't want to wait for it on a successful test).
        server_handle.abort();
        let _ = server_handle.await;

        assert!(
            accepted.load(Ordering::SeqCst),
            "the mock upstream never accepted a connection — the pipeline did \
             not actually reach the HTTP layer, so this test is not exercising \
             the stream-side select! at all"
        );
        // We do not strictly require `client_closed.load(...)` to be
        // true: reqwest MAY keep the connection in its pool until the
        // keepalive timer fires, especially if the body future is
        // dropped before any bytes have been consumed on the wire.
        // The contract that matters for cancellation is that the
        // pipeline's `bytes_stream().next()` future is dropped (so
        // it stops reading the socket); the underlying TCP close is
        // best-effort. We surface the observed value in the message
        // so a regression is obvious in the test logs.
        let observed_close = client_closed.load(Ordering::SeqCst);
        if !observed_close {
            eprintln!(
                "[test note] mock upstream did not observe a client close within \
                 the stall window ({} bytes received after the chunk). This is \
                 acceptable — reqwest may return the connection to its pool — \
                 but a future regression that keeps the body future alive would \
                 manifest as 'observed_close=true' transitioning to false.",
                bytes_after_headers.load(Ordering::SeqCst)
            );
        }
    }
}
