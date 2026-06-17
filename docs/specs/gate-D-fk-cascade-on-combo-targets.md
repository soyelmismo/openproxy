# Gate D — Add ON DELETE SET NULL to combo_targets.model_row_id

## Goal
Make `models` rows deletable by `upsert_many`'s "delete on disappear"
branch (Gate B) even when a `combo_targets` row references them. The
`combo_targets.model_row_id` foreign key currently defaults to
`ON DELETE RESTRICT` in SQLite, which aborts the upsert transaction
the first time a referenced model disappears from an upstream.

## Context (current state)
- Gate B's `upsert_many` DELETEs rows of the provider whose
  `model_id` is not in the discovered diff.
- If any `combo_targets` row still references the about-to-be-
  deleted `models.id`, the DELETE aborts the transaction (and
  with it, the entire upsert of N new rows for the same provider).
- Gate C's E2E test documents this in step 9 and reproduces the
  post-fix state by manually running the DELETE with
  `PRAGMA foreign_keys = OFF`. The proper fix is to add
  `ON DELETE SET NULL` to the FK in the migration, not to leave
  the manual workaround in the test.

## Functional requirements
1. **Add a new migration** in
   `crates/openproxy-core/src/db/migrations.rs` that recreates
   `combo_targets` (or runs `ALTER TABLE` to drop & recreate
   the FK constraint) so the new FK reads:
   ```sql
   model_row_id INTEGER REFERENCES models(id) ON DELETE SET NULL
   ```
   The other columns and indexes stay identical. Pick the
   appropriate SQLite primitive — see Implementation notes.
2. **The migration must be additive**: rows already pointing to
   a `model_row_id` that gets deleted afterwards must end up
   with `model_row_id = NULL`, not block the delete. Verify
   this manually with a one-off `sqlite3` session if needed.
3. **Update the test file**
   `crates/openproxy-server/tests/e2e_models_discovery.rs`:
   - Remove the `PRAGMA foreign_keys = OFF` workaround in step
     9.f.
   - Remove the `assert!(refresh_returns_err)` / 9.e abort
     block.
   - The expected behaviour is now: refresh succeeds, the
     `combo_targets` row gets its `model_row_id` set to NULL,
     and the `list_targets_with_model` query returns
     `model_id = ""` (or NULL) for that target.
4. **No code change to `upsert_many`** — the FK fix alone is
   enough to unblock the DELETE.

## Implementation notes
- SQLite has limited `ALTER TABLE` support. The canonical way
  to change a column's FK is:
  1. `PRAGMA foreign_keys = OFF;` (required; SQLite ignores FK
     changes otherwise)
  2. `BEGIN;`
  3. `CREATE TABLE combo_targets_new ( ... same shape, but
     model_row_id REFERENCES models(id) ON DELETE SET NULL
     ... );`
  4. `INSERT INTO combo_targets_new SELECT * FROM combo_targets;`
  5. `DROP TABLE combo_targets;`
  6. `ALTER TABLE combo_targets_new RENAME TO combo_targets;`
  7. Recreate the indexes (`idx_combo_targets_combo`,
     `idx_combo_targets_model` — check the migration for
     their exact names).
  8. `COMMIT;`
  9. `PRAGMA foreign_keys = ON;` (so subsequent statements in
     the test session see FKs enabled)
  10. `PRAGMA foreign_key_check;` (sanity: returns no rows
      if no orphans were created)
- Wrap the whole thing in a transaction so a partial rebuild
  can never leave `combo_targets` in a broken state.
- The migration version number follows the existing sequence
  in `migrations.rs` (next integer after the last one — check
  the file before assigning).
- The migration must be **idempotent in the sense that
  re-running it on a DB that already has the new shape is
  safe** — the migrations framework already runs each
  migration once via the `schema_migrations` table, so this is
  automatic; just don't write a migration that breaks if a row
  in `combo_targets` already has `model_row_id = NULL`.

## Test requirements
- The Gate C E2E test, simplified as above, must pass.
- Add a tiny unit test next to the migration in `migrations.rs`
  (or in a new `migrations_combo_targets_fk.rs` if the
  existing module is full) that:
  1. Inserts a `combo_targets` row referencing a `models` row.
  2. Deletes the `models` row.
  3. Asserts the `combo_targets` row still exists with
     `model_row_id IS NULL`.
- `cargo test -p openproxy-core` passes.
- `cargo test -p openproxy-server` passes.

## Acceptance criteria
1. `cargo test --workspace` is green.
2. `grep -rn 'PRAGMA foreign_keys = OFF' crates/` returns
   only the test file (which no longer uses the workaround
   for this purpose) or 0 hits if the test was also updated
   cleanly.
3. The new migration appears in `migrations.rs` with a
   version number one higher than the previous one.
4. The Gate C E2E test no longer contains the
   `foreign_keys = OFF` workaround or the "refresh aborts"
   assertion. Step 9 of the spec is now a clean assertion
   that the model is gone and the combo target survived
   with `model_row_id = NULL`.

## Out of scope
- Changing any other FK in the schema.
- Cascading other `combo_targets` columns.
- Touching Gate A / B / C branches.
