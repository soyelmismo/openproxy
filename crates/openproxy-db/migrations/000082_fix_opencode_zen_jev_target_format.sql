-- 000082_fix_opencode_zen_jev_target_format.sql
-- Ensure jev models in opencode-zen and opencode-go use systemone target_format and decision model_type.
UPDATE models
SET target_format = 'systemone', model_type = 'decision'
WHERE provider_id IN ('opencode-zen', 'opencode-go')
  AND (model_id LIKE '%jev%' OR model_id LIKE '%systemone%');
