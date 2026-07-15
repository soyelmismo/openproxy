-- 000016_add_subcombo_support.sql
-- Combo-in-combo: a combo target can now reference either a model (the
-- existing behavior) or another combo (a "sub-combo"). At most one of
-- `model_row_id` and `sub_combo_id` is non-NULL per row; the XOR is
-- enforced in Rust code in `combos::add_target` because SQLite cannot
-- add a CHECK constraint to a populated table.
--
-- Two schema changes are required to allow this:
--
-- 1. `model_row_id` must become nullable (it was `NOT NULL` in
--    migration 000001). A plain `ALTER TABLE … DROP NOT NULL` does
--    not exist in SQLite, so we rebuild the table via the
--    "12-step ALTER TABLE" dance: create a new table with the
--    desired shape, copy the rows over, drop the old one, rename
--    the new one back. The new shape also adds the `sub_combo_id`
--    column and a CHECK that exactly one of the two id columns is
--    set — this is now safe to add because the new table is empty
--    at the moment of the constraint creation.
--
-- 2. A partial index on `sub_combo_id` so cycle-detection probes
--    in `combos::combo_in_chain` and the `list_targets` JOIN don't
--    have to scan NULL rows.

-- Phase 1: rebuild combo_targets with model_row_id NULL-able, a
-- new sub_combo_id column, and a CHECK enforcing the XOR.

CREATE TABLE combo_targets_new (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  combo_id        INTEGER NOT NULL REFERENCES combos(id) ON DELETE CASCADE,
  provider_id     TEXT NOT NULL REFERENCES providers(id),
  account_id      INTEGER REFERENCES accounts(id),
  model_row_id    INTEGER REFERENCES models(id),
  sub_combo_id    INTEGER REFERENCES combos(id) ON DELETE CASCADE,
  priority_order  INTEGER NOT NULL,
  CHECK ((model_row_id IS NOT NULL) <> (sub_combo_id IS NOT NULL)),
  UNIQUE(combo_id, account_id, model_row_id)
);

INSERT INTO combo_targets_new
  (id, combo_id, provider_id, account_id, model_row_id, sub_combo_id, priority_order)
  SELECT id, combo_id, provider_id, account_id, model_row_id, NULL, priority_order
    FROM combo_targets;

DROP TABLE combo_targets;

ALTER TABLE combo_targets_new RENAME TO combo_targets;

-- Phase 2: re-create the indexes that 000001 created on the old
-- table (they were attached to `combo_targets` and don't survive
-- the rename).

CREATE INDEX idx_combo_targets_combo ON combo_targets(combo_id, priority_order);
CREATE INDEX IF NOT EXISTS idx_combo_targets_sub_combo_id
  ON combo_targets(sub_combo_id) WHERE sub_combo_id IS NOT NULL;

