-- 000021_add_none_auth_type.sql
-- Add 'none' to the providers.auth_type CHECK constraint to support
-- anonymous-access providers.

PRAGMA foreign_keys = OFF;

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
  CHECK (auth_type IN ('bearer', 'x-api-key', 'goog-api-key', 'oauth', 'none')),
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

PRAGMA foreign_keys = ON;
