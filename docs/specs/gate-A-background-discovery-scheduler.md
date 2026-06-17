# Gate A — Background Model Discovery Scheduler

## Goal
Auto-refresh the model catalog for every built-in provider on a recurring
interval, so that `GET /v1/models` always reflects what each upstream
currently serves, without requiring the operator to click "Refresh" in
the dashboard.

## Context (current state)
- Endpoint `POST /v1/admin/models/:id/refresh` (and a provider-level
  variant) already exists and works. It calls
  `openproxy_core::admin::refresh_models(provider, conn, api_key, adapter,
  upstream_client, ttl_seconds)` which internally invokes
  `adapter.fetch_models(...)` and `models::upsert_many(...)`.
- `state.rs::new` only spawns two background tasks: a cooldown pruner
  (60s tick) and an OAuth refresh scheduler (60s tick). **There is no
  model-discovery scheduler.**
- Default TTL used by the admin handler is `3_600` seconds (1h). We
  keep that default for the auto-refresh path.
- Adapters are `Arc<dyn ProviderAdapter>`. The list lives in
  `state.adapters` as `Arc<Vec<Arc<dyn ProviderAdapter>>>`.
- Accounts are in the `accounts` table; the provider has one or more
  accounts. `refresh_models` picks an account internally
  (explicit `account_id` query param, otherwise the first healthy
  account).
- Built-in provider ids are listed in
  `openproxy_core::seed::builtin_provider_ids()`. Some of them
  (antigravity, kiro) are OAuth-only and may have 0 accounts at
  startup; the scheduler must skip those gracefully.

## Functional requirements
1. **Cadence**: every built-in provider is refreshed every
   `1 hour` (3600s). The value must be a `const` near the scheduler
   code so it's easy to bump. Name it
   `DISCOVERY_INTERVAL_SECS: u64 = 3600`.
2. **Stagger**: providers do not all fire at the same instant. On
   boot, pick a per-provider initial delay uniformly in
   `[0, DISCOVERY_INTERVAL_SECS)`. After the first run, each provider
   ticks on its own `1h` interval. This avoids a thundering-herd
   request to the dashboard / N upstreams at exactly t=0.
3. **Scope**: only the providers returned by
   `seed::builtin_provider_ids()` are auto-refreshed. Custom
   (operator-created) providers stay manual-only for now.
4. **Account selection**: reuse the same logic as the admin handler
   — pick the first healthy account of the provider. If the
   provider has zero accounts, log a `tracing::info!` and skip
   silently. Do not retry, do not error.
5. **TTL on upsert**: pass `ttl_seconds = 0` (meaning "never
   expire" — see Gate B for why the TTL column is no longer the
   visibility gate, but we still write `expires_at` as a metadata
   hint). Concretely, set
   `expires_at = datetime('now', '+' || 0 || ' seconds')` = now,
   i.e. effectively "no expiry". Note: passing `0` to
   `upsert_many` is already supported and yields an `expires_at`
   one second in the past; Gate B will fix that. For Gate A,
   pass `3600` (1h) for now, so a missed refresh still leaves a
   visible catalog for an hour.
6. **Concurrency**: each provider runs in its own `tokio::spawn`
   task. The task body is an `async { loop { … } }`. Tasks are
   owned by the scheduler struct; when the scheduler is dropped
   the tasks are not cancelled (they hold only `Arc<DbPool>` and
   `Arc<UpstreamClient>` and exit naturally on next tick after
   the pool is unreachable).
7. **Shutdown**: a `CancellationToken` (from `tokio_util`) is
   stored in the scheduler and observed in each loop. When the
   token fires, the task logs `"discovery scheduler for
   {provider} shutting down"` and returns. The token is held in
   the scheduler struct so the `AppState` can call `.cancel()` on
   Drop in the future (no caller-side wiring required for Gate A,
   just wire it).
8. **Logging**: every refresh tick logs at INFO level:
   `provider`, `touched`, `duration_ms`, and on error the error
   message. On skip (no accounts), log at DEBUG level so a
   verbose operator can see the cycle.
9. **Errors must not kill the loop**. A failed refresh (network
   down, upstream 5xx, bad key) is logged at WARN and the next
   tick runs as scheduled. The loop has no `?` past the
   `refresh_models` call.

## Module placement
- New file: `crates/openproxy-core/src/discovery_scheduler.rs`
- Public items:
  - `pub struct DiscoveryScheduler { … }` (private fields)
  - `pub struct DiscoverySchedulerConfig {
       pub interval_secs: u64,
       pub initial_stagger_secs: u64, // upper bound, inclusive
     }`
  - `pub async fn start(
       db_pool: Arc<DbPool>,
       master_key: Arc<MasterKey>,
       adapters: Arc<Vec<Arc<dyn ProviderAdapter>>>,
       upstream_client: Arc<UpstreamClient>,
       config: DiscoverySchedulerConfig,
     ) -> DiscoveryScheduler`
  - `impl DiscoveryScheduler { pub fn cancel(&self); }`
- `crates/openproxy-core/src/lib.rs` must `pub mod discovery_scheduler;`.
- `state.rs::new` calls `start(...)` and stores the returned
  `DiscoveryScheduler` in `AppState` as a new field
  `pub discovery_scheduler: Arc<DiscoveryScheduler>`.
  (If `AppState` already has a `Vec` of background handles, store
  it there; otherwise a new field is fine.)

## Test requirements (Gate A only)
- A unit test in `discovery_scheduler.rs` that uses an in-memory
  adapter (a tiny `#[async_trait]` mock that returns a fixed
  `Vec<DiscoveredModel>`) and a `DbPool` on a temp dir, and
  verifies:
  - After `interval_secs` worth of ticks (use a `tokio::time::pause()`
    / `advance()` test, or pass a 1-second interval and
    `tokio::time::sleep`), the `models` table contains the
    mock's models.
  - A scheduler that gets `cancel()`-ed stops firing within one
    tick.
- A test that verifies the **no-accounts-skip** path: a provider
  with zero `accounts` rows is iterated over without error and
  produces no `models` rows.
- All tests run with `#[tokio::test]`. No real network. The mock
  adapter lives in the test module only; do not export it.

## Acceptance criteria
1. `cargo test -p openproxy-core discovery_scheduler` passes.
2. `cargo build --release` for the whole workspace succeeds.
3. Booting the server in a smoke run logs one
   `discovery scheduler for {provider} starting` per built-in
   provider on the first 60s, then a `refreshed` log per
   provider every `interval_secs`.
4. After 5 minutes of uptime, the `models` table contains
   discovered rows from at least one provider that previously
   had 0 rows. (Manual smoke test the operator runs; you do
   not need to automate this in Gate A.)
5. No new `unwrap()` or `expect()` in the scheduler loop.

## Out of scope (handled in Gate B / C)
- The TTL-vs-presence semantic change.
- The E2E test for delete-on-disappear.
- Auto-refresh of *custom* (operator-created) providers.
