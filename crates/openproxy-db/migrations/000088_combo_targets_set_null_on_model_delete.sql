-- 000088_combo_targets_set_null_on_model_delete.sql
--
-- Revert combo_targets.model_row_id foreign key from ON DELETE CASCADE
-- back to ON DELETE SET NULL.
--
-- When a model is pruned / removed from a provider catalog, the model
-- row is deleted from `models`, but its target row in `combo_targets` is
-- preserved with `model_row_id = NULL` ("ghost" / missing target).
-- The target is automatically skipped during combo routing (`list_targets`
-- checks `NOT (ct.model_row_id IS NULL AND ct.sub_combo_id IS NULL)`).
-- When the model reappears on subsequent upstream discovery, the
-- Gate F1 reconnection logic automatically rebinds `model_row_id`
-- via `upstream_model_id`.
--
-- Also ensure antseed provider has prune_models = 1 so its models are pruned
-- normally from the provider catalog while combos preserve ghost targets.

PRAGMA foreign_keys = OFF;

-- Backfill any missing upstream_model_id values before recreating table
UPDATE combo_targets
   SET upstream_model_id = (SELECT model_id FROM models WHERE id = combo_targets.model_row_id)
 WHERE upstream_model_id IS NULL AND model_row_id IS NOT NULL;

CREATE TABLE combo_targets_new (
  id                  INTEGER PRIMARY KEY AUTOINCREMENT,
  combo_id            INTEGER NOT NULL REFERENCES combos(id) ON DELETE CASCADE,
  provider_id         TEXT NOT NULL REFERENCES providers(id),
  account_id          INTEGER REFERENCES accounts(id),
  model_row_id        INTEGER REFERENCES models(id) ON DELETE SET NULL,
  sub_combo_id        INTEGER REFERENCES combos(id) ON DELETE CASCADE,
  upstream_model_id   TEXT,
  priority_order      INTEGER NOT NULL,
  weight              INTEGER NOT NULL DEFAULT 1,
  active              INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
  cooldown_mode       TEXT CHECK (cooldown_mode IS NULL OR cooldown_mode IN ('flat', 'exponential', 'none')),
  cooldown_base_secs  INTEGER,
  cooldown_max_secs   INTEGER,
  cooldown_factor     INTEGER,
  thinking_effort     TEXT,
  description         TEXT,
  CHECK (NOT (model_row_id IS NOT NULL AND sub_combo_id IS NOT NULL)),
  UNIQUE(combo_id, account_id, model_row_id)
);

INSERT INTO combo_targets_new
  (id, combo_id, provider_id, account_id, model_row_id, sub_combo_id,
   upstream_model_id, priority_order, weight, active, cooldown_mode,
   cooldown_base_secs, cooldown_max_secs, cooldown_factor, thinking_effort, description)
SELECT id, combo_id, provider_id, account_id, model_row_id, sub_combo_id,
       upstream_model_id, priority_order, weight, active, cooldown_mode,
       cooldown_base_secs, cooldown_max_secs, cooldown_factor, thinking_effort, description
  FROM combo_targets;

DROP TABLE combo_targets;

ALTER TABLE combo_targets_new RENAME TO combo_targets;

CREATE INDEX IF NOT EXISTS idx_combo_targets_combo
  ON combo_targets(combo_id, priority_order);
CREATE INDEX idx_combo_targets_sub_combo_id
  ON combo_targets(sub_combo_id) WHERE sub_combo_id IS NOT NULL;
CREATE INDEX idx_combo_targets_upstream_model_id
  ON combo_targets(provider_id, upstream_model_id)
  WHERE upstream_model_id IS NOT NULL;

UPDATE providers SET prune_models = 1 WHERE id = 'antseed';

PRAGMA foreign_keys = ON;
PRAGMA foreign_key_check;
