-- Index for monthly-by-provider and by-provider analytics queries.
-- Covers (created_at, provider_id) so SQLite can range-seek on the time
-- window and walk providers in order for streaming GROUP BY.
CREATE INDEX IF NOT EXISTS idx_usage_created_provider
    ON usage(created_at, provider_id);
