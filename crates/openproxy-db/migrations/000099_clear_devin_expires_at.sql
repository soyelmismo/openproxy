-- 000099_clear_devin_expires_at.sql
-- Devin session tokens are permanent credentials, but expires_at was recorded
-- from the session JWT's exp claim (or the legacy now+1h fallback), flagging
-- every devin account as eternally expiring and pushing it through refresh
-- paths that can never succeed (no refresh grant exists).
--
-- Clear the fabricated deadline on devin rows that have no refresh token.
-- The scheduler then ignores the account, the pipeline uses the access token
-- as-is, and a genuine rejection still arrives as an upstream 401.

UPDATE accounts
SET expires_at = NULL
WHERE provider_id = 'devin'
  AND auth_type = 'oauth'
  AND refresh_token_encrypted IS NULL;
