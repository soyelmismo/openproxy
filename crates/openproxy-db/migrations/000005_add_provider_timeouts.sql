-- 000005_add_provider_timeouts.sql
CREATE TABLE provider_timeouts (
  provider_id     TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE,
  connect_ms      INTEGER NOT NULL DEFAULT 5000,
  request_send_ms INTEGER NOT NULL DEFAULT 10000,
  total_ms        INTEGER NOT NULL DEFAULT 300000,
  created_at      TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
);
