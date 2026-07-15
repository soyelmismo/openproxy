-- 000030_combo_targets_cascade_on_model_delete.sql
--
-- Change combo_targets.model_row_id FK from ON DELETE SET NULL to
-- ON DELETE CASCADE so that when a model disappears from the upstream
-- catalog and is hard-deleted by `models::upsert_many`, the associated
-- combo_targets rows are removed automatically.
--
-- Previously (000025) the FK used ON DELETE SET NULL, which left
-- orphaned rows with model_row_id = NULL. Those rows were filtered
-- out by `list_targets` but still appeared in the admin UI as
-- "row #null" — a confusing artifact for the operator.
--
-- With ON DELETE CASCADE, the combo_targets rows are deleted atomically
-- when the referenced models row is deleted. The admin UI no longer
-- shows stale entries.
--
-- Gate F1 reconnect logic (`reconnect_orphan_targets`) becomes dead
-- code: there are no orphans to reconnect. It is left in place for
-- forward-compatibility (a future migration could revert to SET NULL
-- if the reconnect feature is re-enabled).

PRAGMA foreign_keys = OFF;

CREATE TABLE combo_targets_new (
  id                  INTEGER PRIMARY KEY AUTOINCREMENT,
  combo_id            INTEGER NOT NULL REFERENCES combos(id) ON DELETE CASCADE,
  provider_id         TEXT NOT NULL REFERENCES providers(id),
  account_id          INTEGER REFERENCES accounts(id),
  model_row_id        INTEGER REFERENCES models(id) ON DELETE CASCADE,
  sub_combo_id        INTEGER REFERENCES combos(id) ON DELETE CASCADE,
  upstream_model_id   TEXT,
  priority_order      INTEGER NOT NULL,
  CHECK (NOT (model_row_id IS NOT NULL AND sub_combo_id IS NOT NULL)),
  UNIQUE(combo_id, account_id, model_row_id)
);

INSERT INTO combo_targets_new
  (id, combo_id, provider_id, account_id, model_row_id, sub_combo_id,
   upstream_model_id, priority_order)
SELECT id, combo_id, provider_id, account_id, model_row_id, sub_combo_id,
       upstream_model_id, priority_order
  FROM combo_targets;

DROP TABLE combo_targets;

ALTER TABLE combo_targets_new RENAME TO combo_targets;

-- Recreate indexes (rename strips them).
CREATE INDEX IF NOT EXISTS idx_combo_targets_combo
  ON combo_targets(combo_id, priority_order);
CREATE INDEX idx_combo_targets_sub_combo_id
  ON combo_targets(sub_combo_id) WHERE sub_combo_id IS NOT NULL;
CREATE INDEX idx_combo_targets_upstream_model_id
  ON combo_targets(provider_id, upstream_model_id)
  WHERE upstream_model_id IS NOT NULL;

PRAGMA foreign_keys = ON;
PRAGMA foreign_key_check;
