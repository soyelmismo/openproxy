# Gate E4 — Align `models::delete()` (admin path) with the Gate D SET NULL semantic

## Goal
Make the admin "delete model" path consistent with the
new SET NULL semantic introduced by Gate D. Currently,
`models::delete()` (the operator-driven delete via the
admin UI) does an explicit `DELETE FROM combo_targets
WHERE model_row_id = ?` *before* deleting the model,
which is inconsistent with the scheduler's path (where
the cascade sets the FK to NULL and leaves the
`combo_targets` row in place).

## Context
- Branch base: `feat/gate-D-fk-cascade` (HEAD = `d223e2a`)
- The function is `crates/openproxy-core/src/models.rs::delete`
  (around lines 879-909 per the reviewer; re-locate with
  `grep -n 'pub fn delete\|pub async fn delete' crates/openproxy-core/src/models.rs`).
- It currently does:
  ```rust
  let tx = conn.unchecked_transaction()?;
  tx.execute("DELETE FROM combo_targets WHERE model_row_id = ?1", [model_row_id])?;
  tx.execute("DELETE FROM models WHERE id = ?1", [model_row_id])?;
  tx.commit()?;
  ```
  with a comment saying *"combo_targets.model_row_id has
  no ON DELETE CASCADE in the current schema; clean
  those rows up first so the FK check on the model
  delete doesn't fire."*
- Post-Gate D the FK is `ON DELETE SET NULL`. The
  pre-emptive delete in `delete()` is no longer
  necessary for the FK to succeed — and it's
  semantically wrong, because it deletes the target
  row entirely instead of leaving it as a NULL
  bookmark.

## Functional requirements

1. **Remove the pre-emptive
   `DELETE FROM combo_targets WHERE model_row_id = ?1`**
   from `models::delete()`.
2. **Update the comment** to reflect the new world:
   *"combo_targets.model_row_id has ON DELETE SET
   NULL (migration 000025); the target row is
   preserved with `model_row_id = NULL` and is
   filtered from routing by
   `combos::list_targets`. No pre-emptive cleanup
   needed."*
3. **Behavior change**: after this gate, the admin
   `delete` path leaves the same `(NULL, NULL)` orphan
   state that the scheduler leaves. The orphan is
   filtered from routing by Gate E3.
4. **Audit log** (if the project has one — search
   `crates/` for `audit_log` or `events`): if a model
   is admin-deleted while it still has combo targets,
   emit an audit event with `kind = "combo_target_orphaned"`
   and `combo_id` / `target_id` references. **Skip
   this step if the project does not have an audit
   subsystem**; we do not introduce one in this
   gate. Verify with
   `grep -rn 'audit_log\|AuditEvent' crates/`.
5. **No schema changes.**
6. **No new public API.**

## Test requirements

Add a unit test in
`crates/openproxy-core/src/models.rs::tests`:

### Test: `delete_model_sets_combo_target_model_row_id_to_null`
1. Set up: `providers` row, `combo` row, `models` row
   `M`, `combo_targets` row `T` with
   `model_row_id = M.id`, `sub_combo_id = NULL`.
2. Call `models::delete(&conn, M.id)`.
3. Assert `M` is gone from `models`.
4. Assert `T` still exists in `combo_targets` with
   `model_row_id IS NULL` and `sub_combo_id IS NULL`.
5. Sanity: also create a second combo target
   `T2` for the same combo pointing to a different
   `M2`; assert `T2` is unchanged after the delete
   of `M`.

(Optional but recommended) update any test that
previously asserted the old behavior to assert the
new behavior. Search `models.rs::tests` and
`combos.rs::tests` for `fn delete_model` or
`fn delete_cascades_to_combo_targets` to find
affected tests.

## Acceptance criteria
1. The new test passes.
2. `cargo test --workspace` is green.
3. `grep -n 'DELETE FROM combo_targets' crates/openproxy-core/src/models.rs` returns
   0 hits in the `delete` function (the only remaining
   hit should be inside the upsert-many branch added
   by Gate B, which is a different statement and
   should not be touched).
4. The pre-emptive delete comment is gone.
5. No schema changes (`migrations.rs` untouched).

## Out of scope
- The orphan filter in routing (covered by Gate E3).
- Adding a new audit log subsystem if one doesn't
  already exist.
- Any change to the scheduler path.

## How to land
- Branch: `git checkout -b fix/gate-E4-admin-delete-alignment feat/gate-D-fk-cascade`
- Commit: `fix(core): gate E4 — align admin `models::delete()` with SET NULL semantic`
- Linear history, no rebase.
