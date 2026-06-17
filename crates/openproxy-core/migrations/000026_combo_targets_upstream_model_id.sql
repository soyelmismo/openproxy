-- 000026_combo_targets_upstream_model_id.sql
-- Gate F1: add `upstream_model_id TEXT` to `combo_targets` so the
-- discovery scheduler (Gate A + Gate B) can re-bind orphaned targets
-- when a model reappears upstream.
--
-- Background (see `docs/specs/gate-F1-orphan-reconnection.md`):
--   When a model disappears upstream for one refresh cycle and then
--   comes back, `upsert_many` (Gate B) hard-deletes the stale `models`
--   row and inserts a new one with a fresh autoincrement id. Gate D's
--   `ON DELETE SET NULL` on `combo_targets.model_row_id` cascades the
--   delete onto any combo target that referenced the vanished model,
--   leaving the target row with `model_row_id = NULL`. Today there is
--   no way to recover — the new `models` row has a different id, so
--   the FK target is permanently orphaned and the operator has to
--   hand-edit the combo. Gate F1 fixes that by recording the upstream
--   `model_id` on the target at creation time and using it to
--   reconnect orphaned targets inside the same `upsert_many`
--   transaction when the model comes back.
--
-- Schema change:
--   - Add `upstream_model_id TEXT` (nullable) to `combo_targets`.
--   - Backfill from `models.model_id` for rows whose `model_row_id`
--     is still non-NULL (LEFT JOIN; surviving targets only).
--   - Rows that already lost their model (Gate D's
--     `ON DELETE SET NULL` cascade) get `upstream_model_id = NULL`:
--     we cannot recover the upstream id of a vanished model from
--     `models` (it's gone), so those orphans must be re-bound by the
--     new in-tx reconnect logic once the model comes back. Rows that
--     were orphans BEFORE this migration will stay orphans forever
--     unless the operator edits them by hand — that's a documented
--     limitation in the spec ("Out of scope: backfill of targets
--     created BEFORE this gate").
--
-- We use the same 12-step ALTER TABLE rebuild dance as migrations
-- 000016 and 000025: SQLite cannot add a column with `REFERENCES`
-- via plain `ALTER TABLE ADD COLUMN`, and we need to keep the FK
-- shape of `model_row_id` (set in 000025) untouched. The CHECK on
-- `model_row_id XOR sub_combo_id` is preserved verbatim.
--
-- The migration runner in `db/migrations.rs` wraps this whole script
-- in a `BEGIN ... COMMIT` (IMMEDIATE) transaction and, if it sees
-- `PRAGMA foreign_keys = OFF` in the SQL, toggles the pragma on the
-- connection before BEGIN and after COMMIT. The pragma calls below
-- are therefore no-ops *inside* the runner's transaction (SQLite
-- documents that `foreign_keys` is a no-op in a transaction) — they
-- exist so the auto-detect picks this migration up, and so the same
-- SQL works if a human runs it from a `sqlite3` REPL.

PRAGMA foreign_keys = OFF;

CREATE TABLE combo_targets_new (
  id                  INTEGER PRIMARY KEY AUTOINCREMENT,
  combo_id            INTEGER NOT NULL REFERENCES combos(id) ON DELETE CASCADE,
  provider_id         TEXT NOT NULL REFERENCES providers(id),
  account_id          INTEGER REFERENCES accounts(id),
  model_row_id        INTEGER REFERENCES models(id) ON DELETE SET NULL,
  sub_combo_id        INTEGER REFERENCES combos(id) ON DELETE CASCADE,
  upstream_model_id   TEXT,
  priority_order      INTEGER NOT NULL,
  CHECK (NOT (model_row_id IS NOT NULL AND sub_combo_id IS NOT NULL)),
  UNIQUE(combo_id, account_id, model_row_id)
);

-- Backfill: copy every column of the existing `combo_targets` table
-- and, where `model_row_id` is still set, look up the upstream
-- `model_id` from `models`. Surviving targets get the value; orphans
-- (`model_row_id IS NULL`) and sub-combo targets
-- (`sub_combo_id IS NOT NULL`) get NULL. The LEFT JOIN is the same
-- shape `combos::list_targets_with_model` already uses; we replicate
-- it here so a hand-rolled migration runner sees the same data the
-- app would.
INSERT INTO combo_targets_new
  (id, combo_id, provider_id, account_id, model_row_id, sub_combo_id,
   upstream_model_id, priority_order)
SELECT ct.id, ct.combo_id, ct.provider_id, ct.account_id, ct.model_row_id,
       ct.sub_combo_id,
       m.model_id,
       ct.priority_order
  FROM combo_targets ct
  LEFT JOIN models m ON m.id = ct.model_row_id;

DROP TABLE combo_targets;

ALTER TABLE combo_targets_new RENAME TO combo_targets;

-- Re-create the indexes attached to the old `combo_targets`
-- (the rename strips them). The shape matches what 000025 left
-- behind. The partial index on `sub_combo_id` is recreated without
-- `IF NOT EXISTS` for symmetry; `idx_combo_targets_combo` keeps the
-- `IF NOT EXISTS` from migration 000016 because the rename strips
-- the index but the file's history has not always been strict about
-- the `IF NOT EXISTS` clause on this one.

CREATE INDEX IF NOT EXISTS idx_combo_targets_combo
  ON combo_targets(combo_id, priority_order);
CREATE INDEX idx_combo_targets_sub_combo_id
  ON combo_targets(sub_combo_id) WHERE sub_combo_id IS NOT NULL;

-- Helper index for Gate F1's reconnect query. The reconnect path in
-- `models::upsert_many` (post-Gate-F1) matches orphan targets by
-- `(provider_id, upstream_model_id)`; this composite index makes the
-- lookup O(log n) instead of a full scan. The index is partial —
-- only rows with `upstream_model_id IS NOT NULL` need to be in it,
-- which keeps it small even on tables with millions of sub-combo
-- rows (which all have `upstream_model_id = NULL`).

CREATE INDEX idx_combo_targets_upstream_model_id
  ON combo_targets(provider_id, upstream_model_id)
  WHERE upstream_model_id IS NOT NULL;

PRAGMA foreign_keys = ON;
PRAGMA foreign_key_check;
