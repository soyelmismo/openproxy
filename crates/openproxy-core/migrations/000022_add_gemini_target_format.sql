-- 000022_add_gemini_target_format.sql
-- Extend the models.target_format CHECK constraint to allow 'gemini'.
-- Needed for Gemini-based providers (gemini, antigravity, antigravity-cli).

PRAGMA foreign_keys = OFF;

CREATE TABLE models_new (
  id                     INTEGER PRIMARY KEY AUTOINCREMENT,
  provider_id            TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
  model_id               TEXT NOT NULL,
  display_name           TEXT,
  target_format          TEXT NOT NULL,
  discovered_at          TEXT NOT NULL DEFAULT (datetime('now')),
  expires_at             TEXT,
  timeout_overrides_json TEXT,
  active                 INTEGER NOT NULL DEFAULT 1
                           CHECK (active IN (0, 1)),
  last_test_status       INTEGER,
  last_test_at           TEXT,
  custom                 INTEGER NOT NULL DEFAULT 0
                           CHECK (custom IN (0, 1)),
  context_length         INTEGER,
  max_output_tokens      INTEGER,
  capabilities_json      TEXT,
  family                 TEXT,
  model_type             TEXT NOT NULL DEFAULT 'chat',
  input_modalities_json  TEXT,
  output_modalities_json TEXT,
  UNIQUE(provider_id, model_id),
  CHECK (target_format IN ('openai', 'anthropic', 'gemini'))
);

INSERT INTO models_new (
  id, provider_id, model_id, display_name, target_format,
  discovered_at, expires_at, timeout_overrides_json, active,
  last_test_status, last_test_at, custom, context_length,
  max_output_tokens, capabilities_json, family, model_type,
  input_modalities_json, output_modalities_json
)
SELECT
  id, provider_id, model_id, display_name, target_format,
  discovered_at, expires_at, timeout_overrides_json, active,
  last_test_status, last_test_at, custom, context_length,
  max_output_tokens, capabilities_json, family, model_type,
  input_modalities_json, output_modalities_json
FROM models;

DROP TABLE models;

ALTER TABLE models_new RENAME TO models;

PRAGMA foreign_keys = ON;

-- Recreate indexes that were on the original table
CREATE INDEX IF NOT EXISTS idx_models_provider ON models(provider_id);
CREATE INDEX IF NOT EXISTS idx_models_expires ON models(expires_at);
