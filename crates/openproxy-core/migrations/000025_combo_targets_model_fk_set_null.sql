-- 000025_combo_targets_model_fk_set_null.sql
-- Gate D: relax the `combo_targets.model_row_id` foreign key so that
-- deleting a row from `models` does not abort the calling transaction,
-- and relax the XOR CHECK so a model-only target whose parent
-- `models` row was deleted can survive with `model_row_id = NULL`.
--
-- The FK was created in migration 000016 as
--   model_row_id INTEGER REFERENCES models(id)
-- (no `ON DELETE` clause), which in SQLite resolves to
-- `ON DELETE NO ACTION` and is implemented as `RESTRICT`. That
-- behaviour blocks Gate B's `upsert_many` "delete on disappear"
-- branch the first time a referenced model is removed from the
-- upstream catalog: the hard-delete of the `models` row aborts
-- the transaction, rolling back the entire upsert and leaving
-- the catalog out of sync with the upstream.
--
-- The fix is to recreate `combo_targets` with
--   model_row_id INTEGER REFERENCES models(id) ON DELETE SET NULL
-- so a vanished model turns into a `model_row_id = NULL`
-- bookkeeping row, which is the same end-state the spec
-- (`combo_targets` row survives, `list_targets_with_model`
-- surfaces it as `model_id = ""`) was already asserting in
-- Gate C's E2E test.
--
-- The original CHECK on the table (added in 000016) was
--   (model_row_id IS NOT NULL) <> (sub_combo_id IS NOT NULL)
-- i.e. exactly one of the two id columns is non-NULL. The
-- `ON DELETE SET NULL` cascade would land model-only targets
-- in a `(NULL, NULL)` state and violate that XOR. The CHECK is
-- therefore relaxed to "at most one non-NULL":
--   NOT (model_row_id IS NOT NULL AND sub_combo_id IS NOT NULL)
-- The "at most one" invariant is the real rule Rust enforces
-- in `combos::add_target` (per the 000016 comment); the
-- "exactly one" half of the XOR was opportunistic, made
-- possible only because the new table was empty at the
-- moment the constraint was created. The bookkeeping row
-- `list_targets_with_model` already surfaces the orphan as
-- `model_id = ""` (LEFT JOIN + COALESCE), so the empty state
-- is well-defined for the admin API.
--
-- SQLite has no `ALTER TABLE ... DROP CONSTRAINT` / no
-- `ALTER COLUMN` for FK changes, so we use the same rebuild
-- dance as migration 000016. The shape, columns, and indexes
-- are preserved exactly; the FK clause on `model_row_id` and
-- the CHECK clause change.
--
-- Note: the migration runner in `db/migrations.rs` wraps this
-- whole script in a `BEGIN ... COMMIT` (IMMEDIATE) transaction
-- and, if it sees `PRAGMA foreign_keys = OFF` in the SQL,
-- toggles the pragma on the connection before BEGIN and after
-- COMMIT. The pragma calls below are therefore no-ops *inside*
-- the runner's transaction (SQLite documents that `foreign_keys`
-- is a no-op in a transaction) — they exist so the auto-detect
-- picks this migration up, and so the same SQL works if a human
-- runs it from a `sqlite3` REPL.

PRAGMA foreign_keys = OFF;

CREATE TABLE combo_targets_new (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  combo_id        INTEGER NOT NULL REFERENCES combos(id) ON DELETE CASCADE,
  provider_id     TEXT NOT NULL REFERENCES providers(id),
  account_id      INTEGER REFERENCES accounts(id),
  model_row_id    INTEGER REFERENCES models(id) ON DELETE SET NULL,
  sub_combo_id    INTEGER REFERENCES combos(id) ON DELETE CASCADE,
  priority_order  INTEGER NOT NULL,
  CHECK (NOT (model_row_id IS NOT NULL AND sub_combo_id IS NOT NULL)),
  UNIQUE(combo_id, account_id, model_row_id)
);

INSERT INTO combo_targets_new
  (id, combo_id, provider_id, account_id, model_row_id, sub_combo_id, priority_order)
  SELECT id, combo_id, provider_id, account_id, model_row_id, sub_combo_id, priority_order
    FROM combo_targets;

DROP TABLE combo_targets;

ALTER TABLE combo_targets_new RENAME TO combo_targets;

-- Re-create the indexes attached to the old `combo_targets`
-- (the rename strips them). The shape matches what 000016
-- left behind; both indexes are recreated without
-- `IF NOT EXISTS` because the old table is gone and so are
-- the indexes that used to ride on it.

CREATE INDEX idx_combo_targets_combo ON combo_targets(combo_id, priority_order);
CREATE INDEX idx_combo_targets_sub_combo_id
  ON combo_targets(sub_combo_id) WHERE sub_combo_id IS NOT NULL;

PRAGMA foreign_keys = ON;
PRAGMA foreign_key_check;
