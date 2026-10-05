-- 000085_provider_direct_first.sql
-- Add direct_first column to providers table:
-- When set to 1 and use_proxies is 1, initial requests use the direct host IP (no proxy assigned).
-- When proxy rotation errors (e.g. 429, connect_error, timeout) appear, a proxy from the pool is assigned.
ALTER TABLE providers ADD COLUMN direct_first INTEGER NOT NULL DEFAULT 0 CHECK(direct_first IN (0, 1));
