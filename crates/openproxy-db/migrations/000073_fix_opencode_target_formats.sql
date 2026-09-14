-- 000073_fix_opencode_target_formats.sql
-- Correct target_format for OpenCode Zen / Go models in SQLite.

-- Anthropic wire format for claude, minimax, qwen
UPDATE models
SET target_format = 'anthropic'
WHERE provider_id IN ('opencode-zen', 'opencode-go')
  AND (
    model_id LIKE '%claude%'
    OR model_id LIKE '%minimax%'
    OR model_id LIKE '%qwen%'
  );

-- Gemini wire format for gemini models
UPDATE models
SET target_format = 'gemini'
WHERE provider_id IN ('opencode-zen', 'opencode-go')
  AND model_id LIKE '%gemini%';

-- Responses wire format for muse-spark, gpt-5, gpt-6, grok
UPDATE models
SET target_format = 'responses'
WHERE provider_id IN ('opencode-zen', 'opencode-go')
  AND (
    model_id LIKE '%muse-spark%'
    OR model_id LIKE '%gpt-5%'
    OR model_id LIKE '%gpt-6%'
    OR model_id LIKE '%grok%'
  );
