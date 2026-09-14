-- 000071_fix_usage_failed_costs.sql
-- Reset estimated costs on failed or aborted attempts.
-- Upstream providers only charge when requests succeed (2xx-3xx).
UPDATE usage
SET cost_usd = 0.0
WHERE (status_code >= 400 OR status_code = 0)
  AND cost_usd > 0.0;
