-- 000114: live tier linkage. The tier is the single source of truth for
-- leveraged keys: api_keys carry the tier reference so a tier edit can
-- re-shape every key that inherited it (propagation, not stale snapshots).
ALTER TABLE api_keys ADD COLUMN tier_id INTEGER REFERENCES credit_tiers(id);
CREATE INDEX IF NOT EXISTS idx_api_keys_tier ON api_keys(tier_id);
