# Gate C — E2E test for discovery + delete-on-disappear

## Goal
Prove end-to-end that the new scheduler + new upsert semantic
behave correctly when the upstream catalog actually changes:
what the upstream lists, the catalog shows; what the upstream
drops, the catalog drops.

## Context
- Gate A added `discovery_scheduler` with the per-provider
  refresh loop.
- Gate B changed `upsert_many` to delete rows that are not
  in the discovered set.
- Gate A's test uses a mock `ProviderAdapter` in-process; it
  does not exercise the real HTTP path or the real
  `refresh_models` orchestration in `admin.rs`.
- Gate B's test exercises `upsert_many` directly.
- Neither covers the full chain: `scheduler tick → admin::
  refresh_models → adapter.fetch_models → upsert_many → list
  returned by /v1/models`.

## What this gate adds
A single end-to-end test, in
`crates/openproxy-server/tests/e2e_models_discovery.rs` (new
file), that:
1. Spins up a mock HTTP server on `127.0.0.1:0` that serves
   `/v1/models` and `/v1/chat/completions` shaped like an
   OpenAI-compatible provider. The mock's `/v1/models` body
   is mutable from the test thread (a `parking_lot::Mutex<Vec<&'static str>>`
   behind an `Arc`).
2. Builds an `AppState` (or the minimum needed: `DbPool`,
   `MasterKey`, `adapters` containing a custom test
   `ProviderAdapter` that points at the mock's base URL,
   `UpstreamClient`) using the `for_test` constructor pattern
   already used in `crates/openproxy-server/src/handlers/admin.rs`
   tests.
3. Inserts a `providers` row for the test provider id, and
   one `accounts` row with a fake API key string.
4. Inserts the test provider id into the
   `builtin_provider_ids` list **at runtime** by constructing
   a `Vec<Arc<dyn ProviderAdapter>>` manually — do NOT modify
   the seed list. The test does not go through
   `start_discovery_scheduler` directly; instead it calls
   `admin::refresh_models(...)` twice with the test
   adapter (this is the same code path the scheduler
   uses; we're just driving it synchronously instead of
   waiting for the interval).
5. **Assertion round 1**: first refresh with
   `discovered = ["a", "b", "c"]`. After the call, a
   `SELECT * FROM models WHERE provider_id = ?` returns
   exactly 3 rows: `a, b, c` with `active = 1`. A query
   equivalent to `models::list_active_all` returns the
   same 3 ids.
6. **Change the upstream**: the test thread mutates the
   mock's catalog to `["a", "b"]` (dropped `c`).
7. **Assertion round 2**: call `admin::refresh_models` again
   with the updated mock. After the call, the `models`
   table contains exactly `a, b` for the test provider;
   `c` is gone.
8. **Sanity**: the same call leaves custom (`custom = 1`)
   rows untouched. Insert a hand-picked row `custom-x`
   with `model_id = "z"` and `custom = 1` for the test
   provider, repeat the refresh, assert `z` is still there.
9. **Sanity**: the `combo_targets` view (or
   `combos::list_targets`) for a combo that referenced `c`
   no longer returns `c` after the second refresh.

## Module / file placement
- New file:
  `crates/openproxy-server/tests/e2e_models_discovery.rs`
  (note: existing tests in this crate use both inline
  `#[cfg(test)]` modules and `tests/` files; either is fine,
  but `tests/` keeps the integration boundary clean).
- The mock server should use the existing
  `axum` router the rest of the workspace uses (already a
  transitive dep of `openproxy-server`); see how
  `crates/openproxy-server/src/router.rs::build_router` is
  built and reuse the `Router::new().route(...)` style.
- The mock adapter implementing `ProviderAdapter` is local
  to the test file; do not export it.
- Use `wiremock` if it's already a dev-dep of
  `openproxy-server`; if not, use a hand-rolled `axum`
  server on a free port (`tokio::net::TcpListener::bind`
  on `127.0.0.1:0`, then read the assigned port from
  `listener.local_addr()`).

## Test requirements
- Single test function, `#[tokio::test]`, that runs all
  nine steps above.
- Uses `openproxy_core::db::DbPool::open` on a temp dir
  (mirroring the pattern in
  `crates/openproxy-server/src/handlers/admin.rs::make_state_with_key`).
- Uses a `MasterKey::from_bytes` test helper if it exists,
  or constructs a fixed 32-byte key directly (search
  `crates/openproxy-core/src/secrets.rs` for a `for_test`
  constructor first; if none, add a
  `#[cfg(test)] pub fn for_test() -> Self` to
  `MasterKey`).
- All state is local to the test; no global mutation.
- The test does NOT exercise real network — the mock is
  bound to `127.0.0.1`.

## Acceptance criteria
1. `cargo test -p openproxy-server --test
   e2e_models_discovery` passes.
2. Running the test in isolation (`cargo test -p
   openproxy-server e2e_models_discovery -- --nocapture`)
   produces no warnings.
3. The test fails loudly if any of the nine assertions is
   false (no `.unwrap_or_default()` / `.ok()` masking
   errors).
4. The test takes < 5 seconds wall-clock (it's all
   in-process; if it takes longer, something is hanging
   on the mock server).
5. No new public API in `openproxy-core` or
   `openproxy-server` (the test-only `MasterKey::for_test`
   exception above is allowed but must be `#[cfg(test)]`).

## Out of scope
- Real network calls to real upstreams.
- Real HTTP load on the live `/v1/models` endpoint of a
  running `openproxy`. The unit-level integration above
  is the verification.
- Dashboard / frontend changes.
- Performance / load testing.
- A second scheduler-cadence test (Gate A already
  covers that).
