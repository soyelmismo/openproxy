# Gate B — Delete-on-Disappear: replace TTL with presence-in-last-refresh

## Goal
Change the model-visibility semantic from "row is visible if it was
discovered within the last `ttl_seconds`" to "row is visible if it
was in the most recent successful refresh of its provider". When the
upstream stops listing a model, the row is deleted by the next
refresh, not parked until an arbitrary expiry date.

## Context (current state)
- `models::upsert_many(conn, provider, discovered, ttl)`:
  - INSERTs new rows with `expires_at = now + ttl`.
  - ON CONFLICT updates mutable metadata only; `discovered_at`
    and `expires_at` are deliberately preserved so the auto-
    activation 60-second recency window keeps working.
  - **Does NOT delete rows that disappeared from the upstream's
    `/models` response.**
- `models::list_active(conn, provider)` and
  `models::list_active_all(conn)` filter on
  `active = 1 AND (expires_at IS NULL OR expires_at > now())`.
- `models::mark_expired(conn)` DELETEs rows with
  `expires_at < now()`. Not currently called on a schedule; the
  comment in `state.rs::new` says "intended to be called
  periodically".
- `mark_expired` and the `expires_at > now()` filter are the
  two pieces that need to be re-thought.

## Functional requirements
1. **`upsert_many` now also deletes disappeared rows.** After the
   INSERT phase, inside the same transaction, run:
   ```sql
   DELETE FROM models
   WHERE provider_id = ?
     AND custom = 0
     AND model_id NOT IN (r,'h1', r,'h2', ... of just-upserted)
   ```
   Build the `IN (...)` list from the `discovered` slice. The
   list is bounded by the upstream's catalog size (typically
   <1000 for OpenAI-compatible APIs) so a literal IN list is
   fine; if `discovered` is empty, the WHERE becomes
   `model_id IN (NULL)` after the binding helper normalizes it
   — handle that case by using `1=0` instead so all non-custom
   rows for the provider are deleted (this matches the
   upstream-says-nothing semantic). The `custom = 0` clause
   preserves operator-curated rows from accidental purge.
2. **`upsert_many` preserves `discovered_at` semantics** as today
   on UPDATE (do NOT touch). On INSERT, keep
   `discovered_at = now()`.
3. **Stop using `expires_at` as a visibility gate.** Change
   `list_active` and `list_active_all` to filter on
   `active = 1` only. The `expires_at` column stays in the
   schema (no migration needed; we just stop reading it in
   the hot path) so older code paths and the column itself
   remain queryable. Update the doc comments on
   `list_active` / `list_active_all` to reflect the new
   semantic ("a row is visible iff it was in the most recent
   successful refresh — i.e. it was either just discovered or
   re-confirmed by an upsert; rows the upstream dropped are
   removed by the upsert itself").
4. **Update `combo_targets` and any other query that filters by
   `expires_at`.** Search the workspace for
   `expires_at > datetime\('now'\)` and replace with the
   `active = 1` only filter, or drop the clause if the
   surrounding context already implies active. The docstring
   on `combos::list_targets` (line ~954) and the
   `combo_targets`-list SQL (line ~999) both have
   `expires_at > datetime('now')` clauses; fix them.
5. **Keep `mark_expired` as a manual cleanup utility** for
   orphan rows (e.g. the provider was deleted; rows orphaned
   by a crash mid-upsert). The function signature stays the
   same; the docstring is updated to say "delete rows that
   haven't been touched by a refresh in >7 days, useful for
   cleanup of orphaned rows after a provider deletion —
   NOT part of the normal hot path". Optionally tighten the
   threshold inside `mark_expired` to `7 days` (we no longer
   need to use it for the 1h window).
6. **Update the `models.rs` module-level doc** (top of file)
   to reflect the new semantic. The current doc still talks
   about "TTL-based expiry/purge cycle" — replace with the
   "presence-in-last-refresh" description and cross-reference
   `discovery_scheduler` (Gate A).
7. **No DB migration.** `expires_at` stays; we just stop
   filtering on it. `discovered_at` keeps its meaning.

## Test requirements
- A unit test in `models.rs` that:
  1. Inserts `m1`, `m2`, `m3` for `provA` via `upsert_many`
     with empty `custom`.
  2. Calls `upsert_many` again with `discovered = [m1, m2]`
     (m3 disappeared).
  3. Asserts: `m1` and `m2` are still there with the
     metadata updated; `m3` is gone.
  4. Asserts: `list_active` returns only `[m1, m2]`.
- A unit test that verifies `custom = 1` rows survive an
  upsert whose diff does not contain them. The "delete
  disappeared" branch must skip `custom = 1`.
- A unit test that verifies `discovered = []` deletes all
  non-custom rows for the provider.
- A unit test that verifies `expires_at` is no longer the
  visibility gate: a row with `expires_at` in the past but
  `active = 1` IS returned by `list_active`. (This is the
  contract change.) Backdate a row by hand, assert visible.
- A unit test for `combos::list_targets` (or whatever the
  call site is) that builds a provider+model+combo, runs
  upsert that drops the model, and asserts the combo no
  longer surfaces the dropped model.

## Acceptance criteria
1. `cargo test -p openproxy-core` passes (full crate suite,
   not just the new tests).
2. `cargo test -p openproxy-server` passes.
3. `cargo build --release` for the whole workspace succeeds.
4. The string `expires_at > datetime` does not appear in
   any SQL query in `crates/` except inside `mark_expired`
   (grep check). `expires_at` references that are comparisons
   to `now()` for visibility filtering are gone.
5. Module-level doc on `models.rs` does not mention
   "TTL-based expiry" or "purge cycle" anywhere.
6. No new `unwrap()` / `expect()` in the changed functions.
7. `discovered_at` is still populated on insert and preserved
   on update (existing test
   `apply_auto_activation_does_not_affect_old_re_upserted_model`
   still passes — it asserts `discovered_at` is preserved).

## Out of scope
- The background scheduler itself (Gate A). This gate
  only changes the storage-layer semantic; Gate A's
  scheduler calls into it.
- The E2E mock-server test (Gate C).
- Changing the `custom` row API.
- Any dashboard / frontend change.
