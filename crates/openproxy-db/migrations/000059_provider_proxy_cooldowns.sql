-- 000059_provider_proxy_cooldowns.sql
CREATE TABLE IF NOT EXISTS provider_proxy_cooldowns (
    provider_id     TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    proxy_id        TEXT NOT NULL REFERENCES free_proxies(id) ON DELETE CASCADE,
    cooldown_until  TEXT NOT NULL,
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (provider_id, proxy_id)
);

CREATE INDEX IF NOT EXISTS idx_provider_proxy_cooldowns_until
    ON provider_proxy_cooldowns(cooldown_until);
