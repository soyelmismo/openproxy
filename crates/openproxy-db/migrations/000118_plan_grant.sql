-- Plan grants: credit born from a plan assignment, not from a payment.
--
-- `tier_id` records which plan funded the lot, so re-applying the same plan
-- is idempotent (no double credit) and the plan's lifetime is auditable.
ALTER TABLE credit_transactions ADD COLUMN tier_id INTEGER;
