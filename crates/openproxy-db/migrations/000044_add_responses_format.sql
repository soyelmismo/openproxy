-- 000044_add_responses_format.sql
-- Add 'responses' to the providers.format and models.target_format CHECK constraints.

PRAGMA foreign_keys = OFF;

-- Update providers
CREATE TABLE providers_new (
  id                     TEXT PRIMARY KEY,
  name                   TEXT NOT NULL,
  base_url               TEXT NOT NULL,
  auth_type              TEXT NOT NULL,
  format                 TEXT NOT NULL,
  extra_headers_json     TEXT,
  auto_activate_keyword  TEXT,
  use_proxies            INTEGER NOT NULL DEFAULT 0 CHECK(use_proxies IN (0, 1)),
  current_proxy_id       TEXT REFERENCES free_proxies(id) ON DELETE SET NULL,
  proxy_rotation_errors  TEXT NOT NULL DEFAULT '429,connect_error,timeout',
  active                 INTEGER NOT NULL DEFAULT 1
                           CHECK (active IN (0, 1)),
  created_at             TEXT NOT NULL DEFAULT (datetime('now')),
  CHECK (auth_type IN ('bearer', 'x-api-key', 'goog-api-key', 'oauth', 'none')),
  CHECK (format IN ('openai', 'anthropic', 'mixed', 'gemini', 'responses'))
);

INSERT INTO providers_new (
  id, name, base_url, auth_type, format,
  extra_headers_json, auto_activate_keyword, use_proxies,
  current_proxy_id, proxy_rotation_errors, active, created_at
)
SELECT
  id, name, base_url, auth_type, format,
  extra_headers_json, auto_activate_keyword, use_proxies,
  current_proxy_id, proxy_rotation_errors, active, created_at
FROM providers;

DROP TABLE providers;
ALTER TABLE providers_new RENAME TO providers;

-- Update models
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
  model_id_normalized    TEXT,
  UNIQUE(provider_id, model_id),
  CHECK (target_format IN ('openai', 'anthropic', 'gemini', 'responses'))
);

INSERT INTO models_new (
  id, provider_id, model_id, display_name, target_format,
  discovered_at, expires_at, timeout_overrides_json, active,
  last_test_status, last_test_at, custom, context_length,
  max_output_tokens, capabilities_json, family, model_type,
  input_modalities_json, output_modalities_json, model_id_normalized
)
SELECT
  id, provider_id, model_id, display_name, target_format,
  discovered_at, expires_at, timeout_overrides_json, active,
  last_test_status, last_test_at, custom, context_length,
  max_output_tokens, capabilities_json, family, model_type,
  input_modalities_json, output_modalities_json, model_id_normalized
FROM models;

DROP TABLE models;
ALTER TABLE models_new RENAME TO models;

-- Recreate index on models
CREATE INDEX idx_models_provider_id ON models(provider_id);

PRAGMA foreign_keys = ON;
