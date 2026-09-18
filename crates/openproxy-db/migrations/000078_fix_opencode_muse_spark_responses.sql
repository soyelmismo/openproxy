-- 000078_fix_opencode_muse_spark_responses.sql
-- Fix muse-spark models in OpenCode Zen and Go to use responses wire format (/responses)
-- and update combo_targets to use the contributor-free model alias for free-tier compatibility.

UPDATE models
SET target_format = 'responses'
WHERE provider_id IN ('opencode-zen', 'opencode-go')
  AND model_id LIKE '%muse-spark%';

UPDATE combo_targets
SET upstream_model_id = 'muse-spark-1.3-contributor-free'
WHERE provider_id IN ('opencode-zen', 'opencode-go')
  AND upstream_model_id = 'muse-spark-1.3';

UPDATE combo_targets
SET upstream_model_id = 'muse-spark-1.2-contributor-free'
WHERE provider_id IN ('opencode-zen', 'opencode-go')
  AND upstream_model_id = 'muse-spark-1.2';
