-- 000115: tiers become the only product ("plans", Miguel 2026-11-12).
-- A tier now carries its own commerce: what the buyer pays, what the buyer
-- gets and how long the plan lives. Credit packages are removed from the
-- purchase flow (the table stays only as the historical reference of old
-- orders); the checkout sells tiers directly.
ALTER TABLE credit_tiers ADD COLUMN price_usd REAL;    -- NULL = not sellable yet (draft)
ALTER TABLE credit_tiers ADD COLUMN credit_usd REAL;   -- NULL = 1:1 (credit == price)
ALTER TABLE credit_tiers ADD COLUMN ttl_days INTEGER NOT NULL DEFAULT 30;
ALTER TABLE credit_orders ADD COLUMN tier_id INTEGER REFERENCES credit_tiers(id);
CREATE INDEX idx_credit_orders_tier ON credit_orders(tier_id);
CREATE INDEX idx_credit_tiers_sellable ON credit_tiers(price_usd) WHERE active = 1 AND price_usd IS NOT NULL;
