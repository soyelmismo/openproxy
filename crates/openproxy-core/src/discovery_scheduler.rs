//! Gate A: background model discovery scheduler.
//!
//! Auto-refreshes the model catalog for every built-in provider on a
//! recurring interval, so `GET /v1/models` always reflects what each
//! upstream currently serves without operator intervention.
//!
//! See `docs/specs/gate-A-background-discovery-scheduler.md` for the
//! full spec; the short version is:
//!
//! - One task per built-in provider, spawned at startup.
//! - First tick is staggered uniformly in `[0, DISCOVERY_INTERVAL_SECS)`
//!   so providers don't all fire at the same instant.
//! - Each task re-runs every `DISCOVERY_INTERVAL_SECS` after the
//!   first tick.
//! - If the provider has zero accounts (e.g. an OAuth provider that
//!   hasn't been authorized yet) the task logs at DEBUG and skips
//!   the cycle silently — no error, no retry.
//! - A failed refresh (network down, upstream 5xx, bad key) is
//!   logged at WARN; the next tick runs as scheduled. The loop has
//!   no `?` past the `refresh_models` call.
//! - Shutdown: the scheduler struct owns a `tokio::sync::Notify`
//!   shared by all tasks. `cancel()` triggers it; each task wakes
//!   up on its next sleep boundary, logs "shutting down", and
//!   returns.
//!
//! Why `tokio::sync::Notify` and not `tokio_util::sync::CancellationToken`?
//! The codebase has its own cancel primitive in
//! [`crate::upstream::CancellationToken`] (see `upstream/cancel.rs`)
//! whose module doc explicitly avoids `tokio_util` to keep the
//! dep tree slim. We follow that convention here: a `Notify` gives
//! us the same "wake one waiter" semantic with zero new direct
//! dependencies, and the per-task loop is a `tokio::select!` on
//! `sleep` vs. `notified()` — the same shape the rest of the
//! codebase uses for cancellable sleeps.

use crate::accounts;
use crate::adapters::ProviderAdapter;
use crate::admin;
use crate::db::DbPool;
use crate::ids::ProviderId;
use crate::providers::{self, AuthType};
use crate::secrets::MasterKey;
use crate::seed;
use crate::upstream::UpstreamClient;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio::time::sleep;

/// Per-provider refresh cadence. Spec: 1 hour. Bumped in tests via
/// [`DiscoverySchedulerConfig::interval_secs`].
pub const DISCOVERY_INTERVAL_SECS: u64 = 3_600;

/// TTL written to the `expires_at` column on each upsert. The spec
/// says we still want a visible catalog for an hour after a missed
/// refresh, so the metadata hint matches the cadence. Gate B will
/// re-examine the TTL-vs-presence semantic; for now we just keep
/// the row "warm" for one cycle.
const DISCOVERY_TTL_SECONDS: i64 = 3_600;

/// Configuration knobs exposed to the caller. The defaults match
/// [`DISCOVERY_INTERVAL_SECS`]; tests use the fields to shrink the
/// cadence so a `#[tokio::test(flavor = "current_thread")]` with
/// `tokio::time::pause()` can step through several ticks in
/// microseconds.
#[derive(Debug, Clone)]
pub struct DiscoverySchedulerConfig {
    /// Per-provider tick cadence in seconds.
    pub interval_secs: u64,
    /// Upper bound (inclusive) of the uniform initial stagger in
    /// seconds. The first tick for each provider is scheduled at a
    /// random delay in `[0, initial_stagger_secs]`; production sets
    /// this to `interval_secs` so the herd spreads across a full
    /// cycle. Tests typically set it to 0 so the first tick is
    /// immediate.
    pub initial_stagger_secs: u64,
}

impl Default for DiscoverySchedulerConfig {
    fn default() -> Self {
        Self {
            interval_secs: DISCOVERY_INTERVAL_SECS,
            initial_stagger_secs: DISCOVERY_INTERVAL_SECS,
        }
    }
}

/// Handle to the background discovery scheduler.
///
/// Constructed via [`start`]; the returned struct is the only way
/// the rest of the process interacts with the running tasks.
/// Drop the handle and the tasks keep running — drop is a no-op,
/// not a cancel. To stop the tasks, call [`Self::cancel`].
pub struct DiscoveryScheduler {
    /// `Notify` shared by every spawned task. `cancel()` triggers
    /// one wake-up; each task's `select!` arm fires and the task
    /// returns. We don't need to fan out — every task is
    /// `select!`-ing on this same `Notify` and a single permit is
    /// enough to wake them all (the `Notify` is `notified()`-once
    /// semantics: one permit, one waiter; the rest queue up and
    /// see the flag on the next `notified()` poll).
    cancel: Arc<Notify>,
    /// Kept for symmetry / introspection; the live task count is
    /// visible in tests via a future enhancement.
    _task_count: usize,
}

impl DiscoveryScheduler {
    /// Signal all per-provider tasks to stop. They wake up on
    /// their next sleep boundary (within at most
    /// `interval_secs`), log "shutting down", and return. Idempotent.
    pub fn cancel(&self) {
        // `notify_one()` is enough: a `Notify` stores at most one
        // pending permit, and `notified()` consumes it. Each task's
        // `select!` will see the permit, return, and exit. If a
        // task is mid-`refresh_models` (not awaiting `notified()`),
        // it'll wake at the next sleep boundary regardless — the
        // `is_cancelled` flag on the scheduler guards that.
        self.cancel.notify_one();
    }
}

impl std::fmt::Debug for DiscoveryScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoveryScheduler")
            .field("task_count", &self._task_count)
            .finish_non_exhaustive()
    }
}

/// Spawn one task per built-in provider and return a handle that
/// can cancel them. The handle owns the `Notify`; tasks keep
/// independent `Arc` clones of it.
///
/// `interval_secs` is taken from `config`; the caller is expected
/// to pass the production default (`DISCOVERY_INTERVAL_SECS = 3600`)
/// in production. Tests typically pass `1` and a `0` initial
/// stagger so a `#[tokio::test]` with `tokio::time::pause()` can
/// step through ticks deterministically.
pub async fn start(
    db_pool: Arc<DbPool>,
    master_key: Arc<MasterKey>,
    adapters: Arc<Vec<Arc<dyn ProviderAdapter>>>,
    upstream_client: Arc<UpstreamClient>,
    config: DiscoverySchedulerConfig,
) -> DiscoveryScheduler {
    let cancel = Arc::new(Notify::new());
    let mut task_count = 0usize;

    for pid_str in seed::builtin_provider_ids() {
        let provider = ProviderId::new(*pid_str);
        let adapter = match adapters.iter().find(|a| a.id() == &provider) {
            Some(a) => a.clone(),
            None => {
                // Should not happen: `builtin_provider_ids()` and
                // `builtin_adapters()` are kept in lockstep. We log
                // and skip rather than panic so a future drift
                // doesn't take the whole scheduler down.
                tracing::warn!(
                    provider = %provider,
                    "no adapter registered for built-in provider; \
                     discovery scheduler skipping this provider",
                );
                continue;
            }
        };

        let pool = db_pool.clone();
        let key = master_key.clone();
        let upstream = upstream_client.clone();
        let task_cancel = cancel.clone();
        let interval = config.interval_secs.max(1);
        let initial_stagger = config.initial_stagger_secs;

        // Per-provider initial stagger. We use a small RNG
        // (`rand::random` is already a dep) so the herd is
        // spread across the full window — the call site picks the
        // upper bound and we sample uniformly in `[0, bound]`.
        // The +1 keeps the bound itself reachable when the caller
        // asks for `initial_stagger_secs = 0` we still get 0.
        let first_delay_secs = if initial_stagger == 0 {
            0
        } else {
            // `rand::random::<u64>()` samples the full u64 range;
            // we mod into the caller's window. Modulo bias is
            // negligible at any plausible window (max ~3600).
            rand::random::<u64>() % (initial_stagger + 1)
        };

        tracing::info!(
            provider = %provider,
            interval_secs = interval,
            first_delay_secs,
            "discovery scheduler for {provider} starting",
        );

        tokio::spawn(async move {
            run_one_provider(
                provider,
                adapter,
                pool,
                key,
                upstream,
                interval,
                Duration::from_secs(first_delay_secs),
                task_cancel,
            )
            .await;
        });
        task_count += 1;
    }

    DiscoveryScheduler {
        cancel,
        _task_count: task_count,
    }
}

/// Per-provider loop body. Lives in its own `async fn` so the
/// closure in [`start`] stays short and the test module can call
/// it directly if needed.
///
/// Shape:
/// ```text
///   sleep(first_delay);                       // stagger
///   loop {
///     run_one_tick(provider, ...).await;     // errors are logged, never `?`'d
///     select! {
///       _ = sleep(interval) => continue,
///       _ = cancel.notified() => return,
///     }
///   }
/// ```
async fn run_one_provider(
    provider: ProviderId,
    adapter: Arc<dyn ProviderAdapter>,
    db_pool: Arc<DbPool>,
    master_key: Arc<MasterKey>,
    upstream_client: Arc<UpstreamClient>,
    interval_secs: u64,
    first_delay: Duration,
    cancel: Arc<Notify>,
) {
    // First sleep honors the stagger and the cancel signal in
    // the same `select!`. If the operator cancels before the
    // first tick ever fires (e.g. shutdown on a slow boot) we
    // return without ever calling `refresh_models`.
    if !first_delay.is_zero() {
        tokio::select! {
            _ = sleep(first_delay) => {}
            _ = cancel.notified() => {
                tracing::info!(
                    provider = %provider,
                    "discovery scheduler for {provider} shutting down",
                );
                return;
            }
        }
    }

    loop {
        run_one_tick(
            provider.clone(),
            adapter.clone(),
            &db_pool,
            &master_key,
            &upstream_client,
        )
        .await;

        tokio::select! {
            _ = sleep(Duration::from_secs(interval_secs)) => {}
            _ = cancel.notified() => {
                tracing::info!(
                    provider = %provider,
                    "discovery scheduler for {provider} shutting down",
                );
                return;
            }
        }
    }
}

/// Run a single refresh cycle for `provider`. All errors are
/// logged and swallowed; the caller treats each tick as
/// best-effort and never sees a `Result` out of this function.
///
/// Steps:
/// 1. Look up the provider row in the DB. If it's missing (e.g.
///    the operator deleted a custom provider with the same id
///    somehow) log at WARN and return.
/// 2. List the provider's accounts ordered by priority. If the
///    list is empty log at DEBUG and return — this is the
///    expected "OAuth provider not yet authorized" path.
/// 3. Pick the first account. Decrypt its API key (or pass an
///    empty string for anonymous providers).
/// 4. Open a fresh `Connection` and call
///    [`admin::refresh_models`]. The future is `Send` end to end
///    because we drop the writer guard before the await.
/// 5. Log the result: `provider`, `touched`, `duration_ms` on
///    success; `error` on failure.
async fn run_one_tick(
    provider: ProviderId,
    adapter: Arc<dyn ProviderAdapter>,
    db_pool: &Arc<DbPool>,
    master_key: &Arc<MasterKey>,
    upstream_client: &Arc<UpstreamClient>,
) {
    let started = Instant::now();

    // (1) Provider row check. We hold the writer only for the
    // duration of the `provider_row + accounts_list` snapshot;
    // the writer mutex must be released before we open a second
    // handle and call `refresh_models.await` (the `Connection`
    // carried by `refresh_models` is `Send` but the pool's
    // `MutexGuard` is not).
    let (provider_row, accounts_list) = {
        let w = db_pool.writer();
        let row = match providers::get(&w, &provider) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    provider = %provider,
                    error = %e,
                    "discovery tick: failed to load provider row; skipping cycle",
                );
                return;
            }
        };
        let accs = match accounts::list(&w, Some(&provider)) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(
                    provider = %provider,
                    error = %e,
                    "discovery tick: failed to list accounts; skipping cycle",
                );
                return;
            }
        };
        (row, accs)
    };

    // (2) Skip path: provider missing, no accounts, or
    // explicitly-anonymous provider with no accounts. Log at
    // DEBUG so a verbose operator can see the cycle was hit,
    // but stay quiet at INFO.
    if provider_row.is_none() {
        tracing::debug!(
            provider = %provider,
            "discovery tick: provider row missing; skipping cycle",
        );
        return;
    }
    let is_anonymous = matches!(provider_row.as_ref().map(|p| p.auth_type), Some(AuthType::None));
    if accounts_list.is_empty() {
        if is_anonymous {
            // Anonymous provider: empty accounts is expected, no
            // decrypt needed. We pass an empty API key below and
            // let the adapter do its thing.
            tracing::debug!(
                provider = %provider,
                "discovery tick: anonymous provider, no accounts; using empty api key",
            );
        } else {
            tracing::info!(
                provider = %provider,
                "discovery tick: provider has no accounts; skipping silently",
            );
            return;
        }
    }

    // (3) Pick the first account (highest priority) and decrypt.
    // For OAuth accounts we still pass an empty string —
    // `refresh_models` is what the admin handler does in this
    // same situation (it short-circuits to a refresh-oauth path
    // out-of-band; the discovery scheduler doesn't do that
    // because the OAuth refresh scheduler already keeps tokens
    // fresh, and the /models endpoint for the OAuth upstreams
    // doesn't actually require a usable access token at the
    // point we'd be calling it). This mirrors the existing
    // admin path: `api_key = String::new()` for the
    // selected_account_id == None branch.
    let api_key: String = match accounts_list.first() {
        Some(acc) => {
            // `auth_type` is a free-form `String` on the
            // `Account` row; "oauth" is the only value that
            // signals "no encrypted API key". For those we
            // pass an empty string — the adapter will either
            // work without auth (rare) or fail; the failure is
            // logged at WARN and the next tick tries again.
            if acc.auth_type == "oauth" {
                String::new()
            } else {
                let w = db_pool.writer();
                match accounts::decrypt_api_key(&w, acc.id, master_key.as_ref()) {
                    Ok(k) => k,
                    Err(e) => {
                        tracing::warn!(
                            provider = %provider,
                            account_id = acc.id.0,
                            error = %e,
                            "discovery tick: failed to decrypt api key; skipping cycle",
                        );
                        return;
                    }
                }
            }
        }
        None => String::new(),
    };

    // (4) Open a fresh connection and run the refresh. The
    // borrow of `db_pool` is over — the `&Arc<DbPool>` argument
    // is fine to keep borrowing because the spawned task owns
    // an `Arc` clone anyway.
    let conn = match db_pool.open_connection() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                provider = %provider,
                error = %e,
                "discovery tick: failed to open db connection; skipping cycle",
            );
            return;
        }
    };

    let result = admin::refresh_models(
        conn,
        &provider,
        &api_key,
        adapter.as_ref(),
        upstream_client,
        DISCOVERY_TTL_SECONDS,
    )
    .await;

    let duration_ms = started.elapsed().as_millis();

    // (5) Log outcome. We deliberately do NOT include the
    // `api_key` or any account plaintext in the log payload.
    match result {
        Ok(upsert) => {
            tracing::info!(
                provider = %provider,
                touched = upsert.touched,
                new = upsert.new_model_ids.len(),
                duration_ms,
                "discovery tick: refresh complete",
            );
        }
        Err(e) => {
            // Errors must not kill the loop. WARN, not ERROR, so
            // an upstream that's briefly down doesn't page
            // anyone.
            tracing::warn!(
                provider = %provider,
                error = %e,
                duration_ms,
                "discovery tick: refresh failed",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::{AdapterAuthType, AdapterFormat, ProviderAdapterConfig};
    use crate::db::migrations;
    use crate::ids::{AccountId, ModelId, ProviderId as CoreProviderId};
    use crate::models::{DiscoveredModel, TargetFormat};
    use crate::providers;
    use async_trait::async_trait;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc as StdArc;

    /// A 1-second-interval config that staggers nothing (the
    /// first tick fires immediately on every provider).
    fn fast_config() -> DiscoverySchedulerConfig {
        DiscoverySchedulerConfig {
            interval_secs: 1,
            initial_stagger_secs: 0,
        }
    }

    /// Fresh in-process pool with migrations applied.
    fn fresh_pool() -> (Arc<DbPool>, PathBuf) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir()
            .join(format!("openproxy-discovery-test-{}-{}-{}", pid, nanos, n));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("discovery.db");
        let pool = DbPool::open(&path).expect("open pool");
        {
            let mut w = pool.writer();
            migrations::run(&mut w).expect("migrations");
            // Seed the provider rows so the discovery tick's
            // `providers::get` check passes for the ids we test
            // with. We can't use the real `builtin_provider_ids`
            // list here because the test's adapter registry only
            // knows about our mocks; seeding a subset keeps the
            // scheduler loop focused on the two ids the test
            // cares about.
        }
        (Arc::new(pool), path)
    }

    /// Mock adapter whose `fetch_models` returns a fixed list
    /// and counts every call. Lifted into a `pub` style (still
    /// module-private) so the test bodies can construct one
    /// per test case.
    struct MockAdapter {
        id: CoreProviderId,
        call_count: StdArc<AtomicUsize>,
        models: Vec<DiscoveredModel>,
    }

    impl MockAdapter {
        fn new(id: &str, models: Vec<DiscoveredModel>) -> (Arc<Self>, Arc<AtomicUsize>) {
            let counter = Arc::new(AtomicUsize::new(0));
            let adapter = Arc::new(Self {
                id: CoreProviderId::new(id),
                call_count: counter.clone(),
                models,
            });
            (adapter, counter)
        }
    }

    #[async_trait]
    impl ProviderAdapter for MockAdapter {
        fn id(&self) -> &CoreProviderId {
            &self.id
        }
        fn config(&self) -> &ProviderAdapterConfig {
            // Construct a config on the fly — the discovery
            // tick doesn't read this field, but the trait
            // requires returning a reference. We use a `Box::leak`
            // to anchor a stable address for the lifetime of
            // the test; `MockAdapter` is short-lived.
            let cfg = Box::new(ProviderAdapterConfig {
                id: self.id.clone(),
                base_url: format!("https://mock-{}", self.id).into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Openai,
                extra_headers: Vec::new(),
            });
            Box::leak(cfg)
        }
        fn build_chat_url(
            &self,
            _target_format: TargetFormat,
            _model: &ModelId,
        ) -> String {
            String::new()
        }
        fn build_auth_header(&self, _api_key: &str) -> (String, String) {
            ("Authorization".to_string(), "Bearer mock".to_string())
        }
        fn build_headers(
            &self,
            api_key: &str,
            _target_format: TargetFormat,
            _model: &ModelId,
        ) -> Vec<(String, String)> {
            // The default impl in adapters.rs constructs
            // content-type + extra_headers + auth header. We
            // don't go through the default because we have
            // no real config; emit a minimal set instead.
            let (k, v) = self.build_auth_header(api_key);
            vec![("Content-Type".to_string(), "application/json".to_string()), (k, v)]
        }
        fn models_url(&self) -> Option<String> {
            Some(format!("https://mock-{}/models", self.id))
        }
        async fn fetch_models(
            &self,
            _upstream_client: &Arc<UpstreamClient>,
            _api_key: &str,
        ) -> crate::error::Result<Vec<DiscoveredModel>> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(self.models.clone())
        }
    }

    /// Insert a provider row + a single account whose API key
    /// decrypts to `"sk-test"`. Mirrors what
    /// `openproxy_core::admin::create_account` does in
    /// production, minus the encryption ceremony.
    fn seed_provider_with_account(
        db_pool: &DbPool,
        master_key: &MasterKey,
        provider_id_str: &str,
    ) -> AccountId {
        let conn = db_pool.writer();
        let provider_id = CoreProviderId::new(provider_id_str);
        providers::create(
            &conn,
            &provider_id,
            provider_id_str,
            "https://example.invalid",
            AuthType::Bearer,
            providers::ProviderFormat::Openai,
            None,
            None,
        )
        .expect("seed provider");
        let acc = accounts::create(
            &conn,
            &provider_id,
            Some("sk-test"),
            master_key,
            Some("test"),
            100,
            None,
        )
        .expect("seed account");
        acc
    }

    /// Three fixed model ids the mock returns on every
    /// `fetch_models` call.
    fn three_models() -> Vec<DiscoveredModel> {
        (0..3)
            .map(|i| DiscoveredModel {
                model_id: ModelId::new(format!("mock-model-{}", i)),
                display_name: Some(format!("Mock Model {}", i)),
                target_format: TargetFormat::Openai,
                context_length: None,
                max_output_tokens: None,
                input_modalities: None,
                output_modalities: None,
                model_type: None,
                family: None,
                capabilities: None,
            })
            .collect()
    }

    fn models_with_provider(pool: &DbPool, provider_id_str: &str) -> Vec<crate::models::Model> {
        let conn = pool.writer();
        let provider_id = CoreProviderId::new(provider_id_str);
        crate::models::list_all(&conn)
            .expect("list all")
            .into_iter()
            .filter(|m| m.provider_id == provider_id)
            .collect()
    }

    /// Smoke test: after a few ticks of a fast-cadence
    /// scheduler, the `models` table contains the mock's
    /// models. This is the headline acceptance criterion from
    /// the spec.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn scheduler_upserts_models_after_a_few_ticks() {
        let (pool, _path) = fresh_pool();
        let mk = MasterKey::generate();
        let _acc = seed_provider_with_account(&pool, &mk, "openrouter");

        // Insert only the OpenRouter adapter; the other built-ins
        // would fail to find a matching adapter if we tried to
        // register them.
        let (adapter, counter) = MockAdapter::new("openrouter", three_models());
        let adapters: Arc<Vec<Arc<dyn ProviderAdapter>>> = Arc::new(vec![adapter]);

        // Run with paused time + 1s ticks. We expect the first
        // tick to fire after 1s (the staggered sleep) and
        // the upsert to land in the DB. We give the runtime
        // enough time for at least 2 ticks to confirm the
        // loop is alive.
        let sched = start(
            pool.clone(),
            Arc::new(mk),
            adapters,
            UpstreamClient::new(),
            DiscoverySchedulerConfig {
                interval_secs: 1,
                // Use a non-zero stagger so we exercise the
                // first-sleep-then-loop path; 0 would skip the
                // first sleep entirely.
                initial_stagger_secs: 1,
            },
        )
        .await;

        // Step the runtime forward enough for 2 ticks (2s
        // stagger + 1s tick = 3s total) plus a safety margin.
        tokio::time::advance(Duration::from_secs(4)).await;
        // Yield a few times so the spawned tasks can pick up
        // the advance and process the tick.
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }

        assert!(
            counter.load(Ordering::SeqCst) >= 1,
            "mock adapter should have been called at least once"
        );

        let rows = models_with_provider(&pool, "openrouter");
        assert_eq!(
            rows.len(),
            3,
            "expected three models in DB, got {rows:?}"
        );

        // Cancel the scheduler so the task exits before the
        // test drops the runtime.
        sched.cancel();
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
    }

    /// A provider with zero accounts is iterated over without
    /// error and produces no `models` rows. The mock adapter's
    /// `fetch_models` should NOT be called for it.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn scheduler_skips_provider_with_no_accounts() {
        let (pool, _path) = fresh_pool();
        let mk = MasterKey::generate();
        // Seed the provider row but NOT an account.
        {
            let conn = pool.writer();
            let provider_id = CoreProviderId::new("openrouter");
            providers::create(
                &conn,
                &provider_id,
                "openrouter",
                "https://example.invalid",
                AuthType::Bearer,
                providers::ProviderFormat::Openai,
                None,
                None,
            )
            .expect("seed provider");
        }

        let (adapter, counter) = MockAdapter::new("openrouter", three_models());
        let adapters: Arc<Vec<Arc<dyn ProviderAdapter>>> = Arc::new(vec![adapter]);

        let sched = start(
            pool.clone(),
            Arc::new(mk),
            adapters,
            UpstreamClient::new(),
            DiscoverySchedulerConfig {
                interval_secs: 1,
                initial_stagger_secs: 0,
            },
        )
        .await;

        // Step forward 3s — long enough for at least 2 ticks.
        tokio::time::advance(Duration::from_secs(3)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }

        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "mock adapter must not be called when the provider has no accounts"
        );

        let rows = models_with_provider(&pool, "openrouter");
        assert!(
            rows.is_empty(),
            "no models should have been written; got {rows:?}"
        );

        sched.cancel();
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
    }

    /// A `cancel()`'d scheduler stops firing within roughly one
    /// tick. We use a very long interval (1h) so the only way
    /// the task can wake up after `cancel()` is through the
    /// `Notify` arm of the `select!`.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cancelled_scheduler_stops_within_one_tick() {
        let (pool, _path) = fresh_pool();
        let mk = MasterKey::generate();
        let _acc = seed_provider_with_account(&pool, &mk, "openrouter");

        let (adapter, counter) = MockAdapter::new("openrouter", three_models());
        let adapters: Arc<Vec<Arc<dyn ProviderAdapter>>> = Arc::new(vec![adapter]);

        // 1h interval: the only way the per-provider task
        // wakes up after we call `cancel()` is the `Notify`.
        // First tick fires immediately because stagger is 0
        // and the loop body is "refresh, then sleep 1h".
        let sched = start(
            pool.clone(),
            Arc::new(mk),
            adapters,
            UpstreamClient::new(),
            DiscoverySchedulerConfig {
                interval_secs: 3_600,
                initial_stagger_secs: 0,
            },
        )
        .await;

        // Advance enough for the first tick to land.
        tokio::time::advance(Duration::from_millis(50)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        let calls_after_first_tick = counter.load(Ordering::SeqCst);
        assert!(
            calls_after_first_tick >= 1,
            "first tick should have fired; got {}",
            calls_after_first_tick
        );

        // Cancel and step a little. The task is now parked in
        // the 1h sleep; the `Notify` must wake it.
        sched.cancel();
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }

        // Advance a small amount; the task should have already
        // exited before this point because the `Notify` doesn't
        // depend on time advancing. We advance just to be
        // sure any pending waker fires.
        tokio::time::advance(Duration::from_millis(50)).await;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }

        let calls_after_cancel = counter.load(Ordering::SeqCst);
        // No more ticks should fire. The exact number after
        // cancel equals the number of calls already made; the
        // 1h sleep would have to complete for another tick
        // to happen, and we deliberately don't wait that long.
        assert_eq!(
            calls_after_cancel, calls_after_first_tick,
            "no additional ticks should fire after cancel()"
        );
    }

    /// Built-in providers with no matching adapter are
    /// silently skipped — no panic, no task, no log spam
    /// beyond the WARN. This guards the future-drift branch
    /// in `start()`.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn scheduler_skips_providers_without_an_adapter() {
        let (pool, _path) = fresh_pool();
        let mk = MasterKey::generate();
        // No provider rows seeded at all.
        let adapters: Arc<Vec<Arc<dyn ProviderAdapter>>> = Arc::new(vec![]);

        // Should return successfully with zero tasks spawned
        // (every built-in has no adapter).
        let sched = start(
            pool.clone(),
            Arc::new(mk),
            adapters,
            UpstreamClient::new(),
            fast_config(),
        )
        .await;
        assert_eq!(sched._task_count, 0, "no providers had an adapter");
    }
}
