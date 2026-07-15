-- 000012_add_account_quota.sql
-- Per-account quota snapshot for providers that expose a quota endpoint
-- (currently only MiniMax Coding Plan). Every column is NULL-able so
-- non-quota-capable providers (OpenRouter, OpenCode Zen, custom
-- providers) leave the quota fields empty.
--
-- Layout: session quota + weekly quota, each with used/limit/reset
-- triplets, plus a plan name (e.g. "Coding Plan"), a last_fetched_at
-- timestamp, and a fetch_error string the UI can surface when the
-- quota endpoint refused the call.

ALTER TABLE accounts ADD COLUMN quota_session_used INTEGER;
ALTER TABLE accounts ADD COLUMN quota_session_limit INTEGER;
ALTER TABLE accounts ADD COLUMN quota_session_reset_at TEXT;
ALTER TABLE accounts ADD COLUMN quota_weekly_used INTEGER;
ALTER TABLE accounts ADD COLUMN quota_weekly_limit INTEGER;
ALTER TABLE accounts ADD COLUMN quota_weekly_reset_at TEXT;
ALTER TABLE accounts ADD COLUMN quota_plan_name TEXT;
ALTER TABLE accounts ADD COLUMN quota_last_fetched_at TEXT;
ALTER TABLE accounts ADD COLUMN quota_fetch_error TEXT;
