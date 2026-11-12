-- 000117: a plan defines only 5h and weekly caps (Miguel 2026-11-12).
--
-- The monthly limit of a plan IS its credited balance: what the buyer paid
-- bounds what they can spend, so a separate monthly cap was a second source
-- of truth that conflicted with the plan's own credit and lifetime.
-- Drop it from the product (credit_tiers) and from the runtime row
-- (api_key_quotas); 5h and week stay as the only optional windows.
ALTER TABLE credit_tiers DROP COLUMN monthly_usd_cap;
ALTER TABLE api_key_quotas DROP COLUMN monthly_usd_cap;
