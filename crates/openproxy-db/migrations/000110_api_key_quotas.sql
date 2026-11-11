-- 000110: Per-API-key dollar quotas over 5h / weekly / monthly windows.
-- Spend is measured from `usage.cost_usd` of served responses
-- (client_response = 1), i.e. at official model prices.
-- No row for a key  = unlimited (quota system opt-in).
-- A row with NULL cap = that window is disabled (no limit).
CREATE TABLE IF NOT EXISTS api_key_quotas (
    api_key_id        INTEGER PRIMARY KEY REFERENCES api_keys(id) ON DELETE CASCADE,
    monthly_usd_cap   REAL,
    weekly_usd_cap    REAL,
    five_hour_usd_cap REAL,
    updated_at        TEXT NOT NULL DEFAULT (datetime('now'))
);
