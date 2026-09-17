-- 000076_fix_muse_spark_target_format.sql
-- Fix muse-spark models in OpenCode Zen and Go to use OpenAI wire format (chat/completions)
-- because OpenCode Zen/Go proxies muse-spark to OpenRouter, which requires messages rather than input.

UPDATE models
SET target_format = 'openai'
WHERE model_id LIKE '%muse-spark%';
