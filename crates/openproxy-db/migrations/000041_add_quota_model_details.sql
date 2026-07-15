-- 000041_add_quota_model_details.sql
-- Add per-model quota details column to the accounts table.
-- Stores a JSON array of ModelQuotaDetail objects (model_id, session_used,
-- session_limit, session_reset_at, remaining_fraction) for providers that
-- expose per-model quota (Antigravity family). NULL for providers that
-- only expose aggregate quota.
ALTER TABLE accounts ADD COLUMN quota_model_details TEXT;
