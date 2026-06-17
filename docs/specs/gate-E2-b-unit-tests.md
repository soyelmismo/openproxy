# Gate E2 — Add the missing unit tests for `models::upsert_many` delete-on-disappear

## Goal
Deliver the 4 unit tests that the spec for Gate B
(`docs/specs/gate-B-delete-on-disappear.md`, §"Test requirements")
asked for but the Gate B BUILDER did not write. These are the
safety net for the storage-layer delete-on-disappear behavior.

## Context
- Branch base: `feat/gate-B-delete-on-disappear` (HEAD = `7110174`).
- All required behavior is already implemented in
  `crates/openproxy-core/src/models.rs::upsert_many`; this gate
  is tests-only.
- Existing tests in `models.rs::tests` (search for
  `fn list_active_excludes_expired` and `fn mark_expired_deletes_old`
  to find the test module) provide patterns to follow: use the
  same in-process `DbPool::open` + tempdir pattern, same
  `conn.execute_batch` for schema setup, same `models::upsert_many`
  / `models::list_active` call style.

## Functional requirements

Add the following 4 unit tests, all in
`crates/openproxy-core/src/models.rs` inside the existing
`#[cfg(test)] mod tests` block:

### Test 1: `upsert_many_deletes_models_dropped_by_upstream`
1. Set up a fresh `Connection` / `DbPool` (use the same
   `make_test_pool()` helper the other tests use, or inline the
   schema if there isn't one).
2. Insert a `providers` row for `prov-a` (use
   `admin::create_provider` or a raw INSERT — match what
   neighboring tests do).
3. Call `models::upsert_many(&conn, "prov-a", &["m1", "m2", "m3"], 3600)`.
4. Assert `models::list_active(&conn, "prov-a")` returns 3 rows
   with ids `[m1, m2, m3]`.
5. Call `models::upsert_many(&conn, "prov-a", &["m1", "m2"], 3600)`
   again (upstream dropped m3).
6. Assert `models::list_active(&conn, "prov-a")` returns exactly
   `[m1, m2]`. Assert `m3` is gone from the table entirely (use
   `SELECT COUNT(*) FROM models WHERE model_id = 'm3'` or
   equivalent).

### Test 2: `upsert_many_preserves_custom_rows_when_not_in_diff`
1. Same setup.
2. Insert a `providers` row for `prov-b`.
3. Call `models::upsert_many(&conn, "prov-b", &["m1"], 3600)` to
   establish one discovered row.
4. Insert a `models` row with `provider_id = "prov-b"`,
   `model_id = "operator-curated"`, `custom = 1`,
   `active = 1`. Use `models::create_custom_model` (or
   whatever the existing public API for custom rows is — search
   the file for `custom = 1` inserts in tests) so the row is
   shape-correct.
5. Call `models::upsert_many(&conn, "prov-b", &["m1", "m2"], 3600)`.
   Note `operator-curated` is NOT in the diff.
6. Assert `models::list_active(&conn, "prov-b")` returns
   `[m1, m2, operator-curated]`. The custom row survives.

### Test 3: `upsert_many_with_empty_discovered_deletes_all_non_custom`
1. Same setup.
2. `providers` row for `prov-c`.
3. `models::upsert_many(&conn, "prov-c", &["m1", "m2"], 3600)`.
4. Insert a custom row for `prov-c` with `model_id = "keep"`,
   `custom = 1`.
5. `models::upsert_many(&conn, "prov-c", &[], 3600)` (upstream
   returned an empty catalog).
6. Assert `models::list_active(&conn, "prov-c")` returns
   `["keep"]` (only the custom row remains).

### Test 4: `expires_at_in_the_past_with_active_1_is_visible`
1. Same setup.
2. `providers` row for `prov-d`.
3. `models::upsert_many(&conn, "prov-d", &["stale"], 3600)`.
4. Backdate the row by hand:
   `UPDATE models SET expires_at = datetime('now', '-1 hour') WHERE model_id = 'stale'`.
5. Assert `models::list_active(&conn, "prov-d")` returns the
   row, even though `expires_at` is in the past. This pins the
   new semantic: visibility is driven by `active`, not by
   `expires_at`.

## Test requirements
- All 4 tests use real `Connection` / `DbPool` from a tempdir;
  no mocks.
- All 4 tests follow the existing assertion style in
  `models.rs::tests` (no `.unwrap_or_default()` masking, use
  `assert_eq!` for collection comparisons).
- The tests run in <100ms each (they're all in-process).
- They use the public API (`upsert_many`, `list_active`,
  `create_custom_model`, etc.) — not direct SQL through
  `conn.execute`.

## Acceptance criteria
1. The 4 new tests pass.
2. `cargo test -p openproxy-core` is green (no other tests
   broken by the new fixtures).
3. `grep -c 'fn upsert_many_deletes_models_dropped_by_upstream\|fn upsert_many_preserves_custom_rows_when_not_in_diff\|fn upsert_many_with_empty_discovered_deletes_all_non_custom\|fn expires_at_in_the_past_with_active_1_is_visible' crates/openproxy-core/src/models.rs`
   returns `4`.
4. No new public API introduced.
5. No new dependencies.

## Out of scope
- Tests for `combos::list_targets` (the spec mentioned one;
  the reviewer judged the E2E in Gate C already covers the
  combo scenario end-to-end and skipped the unit version).
  Don't add it here.

## How to land
- Branch: `git checkout -b test/gate-E2-b-unit-tests feat/gate-B-delete-on-disappear`
- Commit: `test(core): gate E2 — add 4 unit tests for upsert_many delete-on-disappear`
- Keep history linear; don't rebase.
