-- 000001_initial_schema.sql
-- MVP initial schema. See docs/mvp-spec.md §8 (final state) and §9 (migration history).
--
-- This file is the historical "day-zero" schema. Columns that arrived in later
-- migrations (000002, 000003) are NOT defined here. The §8 snippet shows the
-- final post-migration shape; the running state is the union of all applied
-- migrations.

CREATE TABLE providers (
  id                 TEXT PRIMARY KEY,
  name               TEXT NOT NULL,
  base_url           TEXT NOT NULL,
  auth_type          TEXT NOT NULL,
  format             TEXT NOT NULL,
  extra_headers_json TEXT,
  created_at         TEXT NOT NULL DEFAULT (datetime('now')),
  CHECK (auth_type IN ('bearer', 'x-api-key')),
  CHECK (format IN ('openai', 'anthropic', 'mixed'))
);

CREATE TABLE accounts (
  id                 INTEGER PRIMARY KEY AUTOINCREMENT,
  provider_id        TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
  api_key_encrypted  BLOB NOT NULL,
  label              TEXT,
  priority           INTEGER NOT NULL DEFAULT 100,
  extra_config_json  TEXT,
  health_status      TEXT NOT NULL DEFAULT 'healthy',
  rate_limited_until TEXT,
  created_at         TEXT NOT NULL DEFAULT (datetime('now')),
  CHECK (health_status IN ('healthy', 'degraded', 'unhealthy'))
);

CREATE TABLE models (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  provider_id   TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
  model_id      TEXT NOT NULL,
  display_name  TEXT,
  target_format TEXT NOT NULL,
  discovered_at TEXT NOT NULL DEFAULT (datetime('now')),
  expires_at    TEXT,
  UNIQUE(provider_id, model_id),
  CHECK (target_format IN ('openai', 'anthropic'))
);

CREATE TABLE combos (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  name       TEXT NOT NULL UNIQUE,
  strategy   TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  CHECK (strategy IN ('priority', 'round_robin'))
);

CREATE TABLE combo_targets (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  combo_id        INTEGER NOT NULL REFERENCES combos(id) ON DELETE CASCADE,
  provider_id     TEXT NOT NULL REFERENCES providers(id),
  account_id      INTEGER REFERENCES accounts(id),
  model_row_id    INTEGER NOT NULL REFERENCES models(id),
  priority_order  INTEGER NOT NULL,
  UNIQUE(combo_id, account_id, model_row_id)
);

CREATE TABLE usage (
  id                INTEGER PRIMARY KEY AUTOINCREMENT,
  request_id        TEXT NOT NULL,
  trace_id          TEXT NOT NULL,
  attempt           INTEGER NOT NULL DEFAULT 1,
  provider_id       TEXT NOT NULL,
  account_id        INTEGER,
  combo_id          INTEGER,
  model_row_id      INTEGER,
  upstream_model_id TEXT NOT NULL,
  combo_target_id   INTEGER,
  prompt_tokens     INTEGER,
  completion_tokens INTEGER,
  cost_usd          REAL,
  status_code       INTEGER NOT NULL,
  error_msg         TEXT,
  created_at        TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE api_keys (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  key_hash   TEXT NOT NULL UNIQUE,
  label      TEXT,
  created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Note: schema_migrations is created and owned by the migration runner
-- (db/migrations.rs), not by this initial migration. This avoids a
-- "table already exists" race when the runner pre-creates the tracking
-- table before iterating.

CREATE INDEX idx_accounts_provider ON accounts(provider_id);
CREATE INDEX idx_models_provider ON models(provider_id);
CREATE INDEX idx_models_expires ON models(expires_at);
CREATE INDEX idx_combo_targets_combo ON combo_targets(combo_id, priority_order);
CREATE INDEX idx_usage_request ON usage(request_id);
CREATE INDEX idx_usage_created ON usage(created_at);
CREATE INDEX idx_usage_provider_model ON usage(provider_id, upstream_model_id, created_at);
