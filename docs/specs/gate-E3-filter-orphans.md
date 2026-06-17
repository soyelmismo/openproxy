# Gate E3 — Filter `(NULL, NULL)` orphan combo targets from routing

## Goal
Prevent the new orphan state introduced by Gate D
(`combo_targets` rows with both `model_row_id` and
`sub_combo_id` `NULL` after a model is deleted by the
discovery scheduler) from reaching the user as a confusing
`5xx Internal Error: execute_single called on a sub-combo
target`. The orphan rows are still useful for audit
("this combo used to point to model X"), so we **filter
them at the routing layer**, not delete them.

## Context
- Branch base: `feat/gate-D-fk-cascade` (HEAD = `d223e2a`)
- New code path enabled by Gate D: `upsert_many` may now
  successfully DELETE a `models` row that is referenced by
  `combo_targets`. The FK ON DELETE SET NULL on
  `combo_targets.model_row_id` sets that column to NULL;
  the `sub_combo_id` column was already NULL for these
  rows (combo → model targets have only `model_row_id`
  set). The CHECK constraint now allows
  `(model_row_id IS NULL, sub_combo_id IS NULL)`.
- The reviewer found that `routing.rs::resolve` (around
  line 101) returns these rows to `RoutingPlan::Combo`
  without filtering. `pipeline.rs:1180-1184` then
  dispatches them to `execute_single` which returns
  `CoreError::Internal("...sub-combo target...")`.
- The reviewer identified 3 sensible places to add the
  filter. **Pick the one closest to the user-visible
  surface so the orphan never reaches a downstream
  caller**: `combos::list_targets` (or whatever the
  function called from `routing.rs` is — search
  `crates/openproxy-core/src/combos.rs` for the SQL query
  used to populate a combo's targets).

## Functional requirements

1. **Locate the function** that returns a `Combo`'s
   list of targets. It is the function called from
   `routing.rs::resolve` when a `RoutingPlan::Combo` is
   built. Verify with a `grep` chain:
   ```
   grep -n 'fn resolve' crates/openproxy-core/src/routing.rs
   grep -n 'list_targets' crates/openproxy-core/src/routing.rs
   grep -n 'combo_targets' crates/openproxy-core/src/combos.rs
   ```
2. **Add a `WHERE NOT (model_row_id IS NULL AND
   sub_combo_id IS NULL)` clause** to the SQL query in
   that function. (Do NOT add it to every function in
   `combos.rs` — only to the one that powers routing
   decisions. Other callers of `list_targets` may have
   legitimate reasons to see orphans, e.g. the admin
   dashboard.)
3. **Update the docstring** of the modified function to
   note the new filter: *"Orphan targets — rows where
   the upstream model has been deleted by the
   scheduler — are excluded; they remain in the
   table for audit and re-activation when the model
   reappears."*
4. **No other changes to the routing pipeline.** The
   `5xx Internal` path stays untouched; we just stop
   orphans from reaching it.
5. **Do not change the `(model_row_id IS NULL, sub_combo_id IS NULL)` semantics of the `combo_targets` table.** Orphans stay in the DB. The filter is read-time only.

## Test requirements

Add a unit test in `combos::tests` (or wherever the
existing `combos` tests live — search `crates/openproxy-core/src/combos.rs` for `#[cfg(test)] mod tests`):

### Test: `list_targets_excludes_orphan_targets`
1. Set up: `providers` row for `prov-x`, `combo` row
   with `id = 1` referencing `prov-x`, two
   `combo_targets` rows for combo `1`:
   - `target-a`: pointing to a real `models` row
   - `target-orphan`: pointing to a real `models` row
2. Run the routing-target listing function. Capture
   the returned list of targets for combo `1`.
3. Assert `target-a` is in the list.
4. Assert `target-orphan` is NOT in the list.
5. Sanity: query `combo_targets` directly (raw SQL)
   and assert `target-orphan` is still there in the
   table (not deleted by the filter, just excluded
   from the read).

### Test: `list_targets_returns_empty_for_fully_orphaned_combo`
1. Create a combo whose only targets are orphans
   (all `model_row_id = NULL`).
2. Assert the listing function returns an empty
   vec.
3. (Optional but useful) assert that
   `routing::resolve` for this combo returns a
   `RoutingPlan::Empty` or similar — pick whatever
   the codebase already does for a combo with 0
   usable targets. This protects against a future
   refactor that flips the orphan→error semantic
   back on.

## Acceptance criteria
1. The two new tests pass.
2. `cargo test --workspace` is green.
3. `grep -n 'model_row_id IS NULL AND sub_combo_id IS NULL' crates/openproxy-core/src/combos.rs` returns
   at least one new hit (the WHERE clause).
4. `grep -n 'model_row_id IS NULL AND sub_combo_id IS NULL' crates/openproxy-core/src/routing.rs` returns 0 hits (we filter at the data layer, not in routing).
5. No change to the `combo_targets` table schema.
6. The `models::delete()` admin path (pre-existing) is NOT modified by this gate.

## Out of scope
- The `models::delete()` inconsistency (covered by Gate E4).
- Any change to the `combo_targets` schema or the CHECK constraint.
- A UI / dashboard change to show orphan targets in the admin.
- Changing the `CoreError::Internal` message in `pipeline.rs`.

## How to land
- Branch: `git checkout -b fix/gate-E3-filter-orphans feat/gate-D-fk-cascade`
- Commit: `fix(core): gate E3 — filter (NULL,NULL) orphan combo targets from routing`
- Linear history, no rebase.
