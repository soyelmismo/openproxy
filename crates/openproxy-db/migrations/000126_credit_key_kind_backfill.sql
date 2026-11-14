-- Payments-only: backfill the key kind for keys that exist because of a
-- sale (checkout-created or plan-granted). Split from 000125 because it
-- reads `tier_id`, a column that only exists with the payments feature.
-- Idempotent: re-running it re-sets the same kind.
UPDATE api_keys SET kind = 'customer'
 WHERE created_by LIKE 'credits-checkout:%' OR tier_id IS NOT NULL;
