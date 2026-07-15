-- 000043_provider_proxies.sql
-- Add proxy support and rotation settings to the providers table.

ALTER TABLE providers ADD COLUMN use_proxies INTEGER NOT NULL DEFAULT 0 CHECK(use_proxies IN (0, 1));
ALTER TABLE providers ADD COLUMN current_proxy_id TEXT REFERENCES free_proxies(id) ON DELETE SET NULL;
ALTER TABLE providers ADD COLUMN proxy_rotation_errors TEXT NOT NULL DEFAULT '429,connect_error,timeout';
