-- 000067_fix_remaining_model_types.sql
--
-- Heal non-custom models with misclassified model_type:
-- 1. ASR models mistakenly healed to chat:
UPDATE models
SET model_type = 'audio'
WHERE custom = 0
  AND model_type = 'chat'
  AND (
    model_id LIKE '%-asr%'
    OR model_id LIKE '%_asr%'
    OR model_id LIKE '%/asr%'
  );

-- 2. TTS models with :free or other suffixes that were not identified as audio:
UPDATE models
SET model_type = 'audio'
WHERE custom = 0
  AND model_type = 'chat'
  AND (
    model_id LIKE '%-tts%'
    OR model_id LIKE '%_tts%'
    OR model_id LIKE '%/tts%'
  );

-- 3. Dedicated image models mistakenly classified as chat:
UPDATE models
SET model_type = 'image'
WHERE custom = 0
  AND model_type = 'chat'
  AND (
    model_id LIKE '%seedream%'
    OR model_id LIKE '%sdxl%'
    OR model_id LIKE '%grok-imagine%'
    OR model_id LIKE '%nano-banana%'
    OR model_id LIKE '%lucid-origin%'
    OR model_id LIKE '%quiverai/arrow%'
  )
  AND NOT (
    model_id LIKE '%diffusiongemma%'
    OR model_id LIKE '%sdft%'
  );

-- 4. Rerank models mistakenly classified as embedding:
UPDATE models
SET model_type = 'rerank'
WHERE custom = 0
  AND model_type = 'embedding'
  AND model_id LIKE '%rerank%';
