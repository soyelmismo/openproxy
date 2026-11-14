-- Classify every API key as `customer` (sold through the checkout, governed by
-- a plan) or `personal` (created by the operator for own consumption).
--
-- The kind is the only explicit owner signal: scopes cannot tell them apart
-- (a customer key also carries `chat`) and `tier_id` only exists once a plan
-- has been granted. The customer backfill lives in the payments-only
-- migration 000126 because it reads `tier_id`. Everything else starts as
-- personal and can be flipped from the dashboard.
ALTER TABLE api_keys ADD COLUMN kind TEXT NOT NULL DEFAULT 'personal'
    CHECK (kind IN ('personal', 'customer'));

-- The exactly-once debit marker for billable usage rows (credits_billing).
-- Universal: the column always exists; only the debit code is feature-gated.
ALTER TABLE usage ADD COLUMN credits_debited INTEGER NOT NULL DEFAULT 0;

-- Every analytics aggregate filters usage by key kind through this column.
CREATE INDEX IF NOT EXISTS idx_api_keys_kind ON api_keys(kind);
