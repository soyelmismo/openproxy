-- 000019_add_oauth_support.sql
-- Add OAuth support:
-- 1. Rebuild providers table to allow auth_type = 'oauth'
-- 2. Rebuild accounts table to make api_key_encrypted nullable and add OAuth columns

PRAGMA foreign_keys = OFF;

-- === 1. Rebuild providers: add 'oauth' to auth_type CHECK ===

CREATE TABLE providers_new (
  id                     TEXT PRIMARY KEY,
  name                   TEXT NOT NULL,
  base_url               TEXT NOT NULL,
  auth_type              TEXT NOT NULL,
  format                 TEXT NOT NULL,
  extra_headers_json     TEXT,
  auto_activate_keyword  TEXT,
  active                 INTEGER NOT NULL DEFAULT 1
                           CHECK (active IN (0, 1)),
  created_at             TEXT NOT NULL DEFAULT (datetime('now')),
  CHECK (auth_type IN ('bearer', 'x-api-key', 'goog-api-key', 'oauth')),
  CHECK (format IN ('openai', 'anthropic', 'mixed', 'gemini'))
);

INSERT INTO providers_new (
  id, name, base_url, auth_type, format,
  extra_headers_json, auto_activate_keyword, active, created_at
)
SELECT
  id, name, base_url, auth_type, format,
  extra_headers_json, auto_activate_keyword, active, created_at
FROM providers;

DROP TABLE providers;

ALTER TABLE providers_new RENAME TO providers;

-- Update Antigravity providers to use OAuth auth_type (they use OAuth
-- tokens, not static API keys, even though they were originally seeded
-- as 'bearer').
UPDATE providers SET auth_type = 'oauth' WHERE id IN ('antigravity', 'antigravity-cli');

-- === 2. Rebuild accounts: make api_key_encrypted nullable, add OAuth columns ===

CREATE TABLE accounts_new (
  id                          INTEGER PRIMARY KEY AUTOINCREMENT,
  provider_id                 TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
  api_key_encrypted           BLOB,
  label                       TEXT,
  priority                    INTEGER NOT NULL DEFAULT 100,
  extra_config_json           TEXT,
  health_status               TEXT NOT NULL DEFAULT 'healthy'
                               CHECK (health_status IN ('healthy', 'degraded', 'unhealthy')),
  rate_limited_until          TEXT,
  -- quota columns (migration 000012)
  quota_session_used          INTEGER,
  quota_session_limit         INTEGER,
  quota_session_reset_at      TEXT,
  quota_weekly_used           INTEGER,
  quota_weekly_limit          INTEGER,
  quota_weekly_reset_at       TEXT,
  quota_plan_name             TEXT,
  quota_last_fetched_at       TEXT,
  quota_fetch_error           TEXT,
  -- OAuth columns (migration 000019)
  auth_type                   TEXT NOT NULL DEFAULT 'api_key',
  access_token_encrypted      BLOB,
  refresh_token_encrypted     BLOB,
  token_type                  TEXT DEFAULT 'bearer',
  expires_at                  TEXT,
  oauth_scope                 TEXT,
  oauth_provider_specific     TEXT,
  email                       TEXT,
  created_at                  TEXT NOT NULL DEFAULT (datetime('now')),
  CHECK (auth_type IN ('api_key', 'oauth'))
);

INSERT INTO accounts_new (
  id, provider_id, api_key_encrypted, label, priority, extra_config_json,
  health_status, rate_limited_until,
  quota_session_used, quota_session_limit, quota_session_reset_at,
  quota_weekly_used, quota_weekly_limit, quota_weekly_reset_at,
  quota_plan_name, quota_last_fetched_at, quota_fetch_error,
  created_at
)
SELECT
  id, provider_id, api_key_encrypted, label, priority, extra_config_json,
  health_status, rate_limited_until,
  quota_session_used, quota_session_limit, quota_session_reset_at,
  quota_weekly_used, quota_weekly_limit, quota_weekly_reset_at,
  quota_plan_name, quota_last_fetched_at, quota_fetch_error,
  created_at
FROM accounts;

DROP TABLE accounts;

ALTER TABLE accounts_new RENAME TO accounts;

CREATE INDEX idx_accounts_provider ON accounts(provider_id);
CREATE INDEX IF NOT EXISTS idx_accounts_expires_at ON accounts(expires_at) WHERE expires_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_accounts_auth_type ON accounts(auth_type);

PRAGMA foreign_keys = ON;
