# Gate E1 — Replace `Notify` with broadcast-aware cancel for the discovery scheduler

## Goal
Fix the BLOCKER found by the reviewer on `feat/gate-A-discovery-scheduler`:
the `DiscoveryScheduler::cancel()` uses `tokio::sync::Notify::notify_one()`,
which only wakes **one** of the N per-provider tasks on shutdown. With the
11 built-in providers, the other 10 stay parked in their `select!` sleep
until their next tick (up to 1h later).

## Context
- Branch base: `feat/gate-A-discovery-scheduler` (HEAD = `2cbab66`)
- Current code: `crates/openproxy-core/src/discovery_scheduler.rs`
  - `cancel_signal: Arc<Notify>` in the struct
  - In the loop: `tokio::select! { _ = sleep.next().fuse() => ..., _ = cancel_signal.notified() => break }`
  - In `cancel()`: `cancel_signal.notify_one()`
- The comment at lines 95-101 of the module claiming "a single permit
  is enough to wake them all" is factually wrong per
  `tokio::sync::Notify` docs.

## Functional requirements
1. **Use broadcast semantics for the cancel signal.** Pick ONE of:
   a. `tokio::sync::watch::Sender<bool>` + `Receiver::changed().await`
      in the loop. The receiver wakes on every `send(true)`. Closing the
      sender (drop) also wakes all waiters; use that for shutdown.
   b. `tokio::sync::broadcast::Sender<()>` + `Receiver::recv().await`.
      Capacity 1 is fine; senders never block, receivers are lazy.
   c. `tokio_util::sync::CancellationToken`. This is what the original
      spec asked for. `tokio_util` is already a transitive dep in the
      workspace (verify with `cargo tree -i tokio-util`); promoting it
      to a direct dep of `openproxy-core` is the simplest and most
      idiomatic.
2. **Option (c) is the preferred fix** because it matches the spec
   literally. If (c) is chosen, add `tokio-util = { workspace = true,
   features = ["rt"] }` to `openproxy-core/Cargo.toml`. Do not add
   other features of `tokio-util` that aren't required.
3. **Replace all uses of `Notify` in `discovery_scheduler.rs`** with the
   chosen primitive. The `cancel()` method body becomes
   `self.cancel_token.cancel()` (or equivalent). The select! becomes
   `select! { _ = sleep.next().fuse() => ..., _ = cancel_token.cancelled() => break }`.
4. **Update the module-level doc** to remove the misleading "single
   permit is enough" claim.
5. **Update the test `cancelled_scheduler_stops_within_one_tick`** to
   use ALL 11 built-in providers (or at minimum 3-4) instead of 1, so
   it actually exercises the broadcast semantic. Concretely:
   - Replace the single fake adapter with a list of 4 fake adapters
     with distinct `provider_id`s, each registered in a `Vec<Arc<dyn
     ProviderAdapter>>`.
   - Insert a corresponding row per provider into the `providers`
     table.
   - Spin up the scheduler with all 4 adapters.
   - Call `cancel()` and assert all 4 tasks return within a short
     deadline (e.g. 500ms).

## Test requirements
- Updated test from §5 above must pass.
- The 3 other existing scheduler tests must continue to pass without
  modification (the API surface of the struct shouldn't change, just
  the internals).
- `cargo test -p openproxy-core` full suite must be green.
- `cargo test -p openproxy-server` must be green.
- `cargo build --release --workspace` must succeed.

## Acceptance criteria
1. The string `Notify` does not appear in
   `crates/openproxy-core/src/discovery_scheduler.rs` (grep
   verification).
2. The misleading comment at lines 95-101 is gone.
3. The updated cancellation test covers N>=3 providers.
4. No new `unwrap()` / `expect()` in production code (the
   `#[cfg(test)]` mod is exempt).
5. `cargo test --workspace` green.

## Out of scope
- The WARNING-level issues from the reviewer (these belong to
  Gate E2 / E3 / E4).
- Any change to the per-task logic of the scheduler (intervals,
  account selection, error handling). This gate is **only** the
  cancel primitive.

## How to land
- Create branch: `git checkout -b fix/gate-A-broadcast-cancel
  feat/gate-A-discovery-scheduler`
- Commit message: `fix(core): gate E1 — broadcast-aware cancel for
  discovery scheduler`
- DO NOT rebase the underlying Gate A; keep history linear.
