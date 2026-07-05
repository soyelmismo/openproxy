-- 000042_free_proxies.sql
-- Creates the staging table for scraped and manually-added free proxies.

CREATE TABLE IF NOT EXISTS free_proxies (
  id TEXT PRIMARY KEY,
  source TEXT NOT NULL,                  -- 'proxifly' | 'iplocate' | '1proxy' | 'custom'
  host TEXT NOT NULL,
  port INTEGER NOT NULL,
  type TEXT NOT NULL DEFAULT 'http',     -- 'http' | 'https' | 'socks4' | 'socks5'
  country_code TEXT,
  status TEXT NOT NULL DEFAULT 'unknown',-- 'unknown' | 'alive' | 'dead'
  latency_ms INTEGER,
  last_validated TEXT,
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE(host, port)
);

CREATE INDEX IF NOT EXISTS idx_free_proxies_source ON free_proxies(source);
CREATE INDEX IF NOT EXISTS idx_free_proxies_status ON free_proxies(status);
