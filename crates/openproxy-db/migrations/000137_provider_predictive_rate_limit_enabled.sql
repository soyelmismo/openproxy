-- 000137_provider_predictive_rate_limit_enabled.sql
--
-- Per-provider switch for the rate-limit machinery (predictive limiter +
-- account rate_limited_until parking). Default ON (1): existing deployments
-- keep today's behavior.
--
-- Why: a single upstream blip arms accounts.rate_limited_until for the full
-- Retry-After window, and every request refused by the local gate re-arms it
-- again (the refusal reports the full window, not the remaining time), so one
-- 503 can lock a provider for as long as the operator keeps retrying. The
-- per-provider OFF value bypasses arming and gating for that provider's
-- accounts entirely (resolve gate, predictive evaluate/acquire, retry
-- gate, and the result-driven arm), giving the operator a real escape hatch
-- without a restart.

ALTER TABLE providers ADD COLUMN predictive_rate_limit_enabled INTEGER NOT NULL DEFAULT 1;
