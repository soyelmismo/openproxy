-- 000102_combo_targets_cascade_on_provider_delete.sql
--
-- Add ON DELETE CASCADE to combo_targets.provider_id and
-- ON DELETE SET NULL to combo_targets.account_id.
--
-- When a custom provider is deleted, its child combo_targets are removed
-- automatically via CASCADE instead of failing with a foreign key error.
-- When an account is deleted, its combo_targets have account_id set to NULL
-- via SET NULL so the target continues routing across remaining accounts.

PRAGMA foreign_keys = OFF;

CREATE TABLE combo_targets_new (
  id                  INTEGER PRIMARY KEY AUTOINCREMENT,
  combo_id            INTEGER NOT NULL REFERENCES combos(id) ON DELETE CASCADE,
  provider_id         TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
  account_id          INTEGER REFERENCES accounts(id) ON DELETE SET NULL,
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
  selected_accounts   TEXT DEFAULT NULL,
  CHECK (NOT (model_row_id IS NOT NULL AND sub_combo_id IS NOT NULL)),
  UNIQUE(combo_id, account_id, model_row_id)
);

INSERT INTO combo_targets_new
  (id, combo_id, provider_id, account_id, model_row_id, sub_combo_id,
   upstream_model_id, priority_order, weight, active, cooldown_mode,
   cooldown_base_secs, cooldown_max_secs, cooldown_factor, thinking_effort, description, selected_accounts)
SELECT id, combo_id, provider_id, account_id, model_row_id, sub_combo_id,
       upstream_model_id, priority_order, weight, active, cooldown_mode,
       cooldown_base_secs, cooldown_max_secs, cooldown_factor, thinking_effort, description, selected_accounts
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

PRAGMA foreign_keys = ON;
PRAGMA foreign_key_check;
