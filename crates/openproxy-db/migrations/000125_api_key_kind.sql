-- Classify every API key as `customer` (sold through the checkout, governed by
-- a plan) or `personal` (created by the operator for own consumption).
--
-- The kind is the only explicit owner signal: scopes cannot tell them apart
-- (a customer key also carries `chat`) and `tier_id` only exists once a plan
-- has been granted. Existing rows are backfilled from the two facts that do
-- identify a sale: the checkout's `credits-checkout:<order>` creator stamp and
-- a granted tier. Everything else starts as personal and can be flipped from
-- the dashboard.
ALTER TABLE api_keys ADD COLUMN kind TEXT NOT NULL DEFAULT 'personal'
    CHECK (kind IN ('personal', 'customer'));

UPDATE api_keys SET kind = 'customer'
 WHERE created_by LIKE 'credits-checkout:%' OR tier_id IS NOT NULL;

-- Every analytics aggregate filters usage by key kind through this column.
CREATE INDEX IF NOT EXISTS idx_api_keys_kind ON api_keys(kind);
