-- Live quota checks must not scan a key's entire history for every refresh.
-- Covers SUM(cost_usd) and MIN(created_at) for billable rows, with a range
-- seek on the window boundary. No cached balances or relaxed durability.
CREATE INDEX IF NOT EXISTS idx_usage_quota_window
ON usage(api_key_id, created_at, cost_usd) WHERE client_response = 1;
