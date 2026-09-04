-- 000066_fix_multimodal_chat_model_type.sql
--
-- Heal non-custom models mistakenly tagged as 'audio' or 'image' due to historical
-- multimodal input/output classification heuristics.
-- Only non-custom models (custom = 0) are updated.

UPDATE models
SET model_type = 'chat'
WHERE custom = 0
  AND model_type IN ('audio', 'image')
  AND (
    model_id LIKE '%gemini%'
    OR model_id LIKE '%qwen%'
    OR model_id LIKE '%llama%'
    OR model_id LIKE '%claude%'
    OR model_id LIKE '%gpt-%'
    OR model_id LIKE '%glm-%'
    OR model_id LIKE '%deepseek%'
    OR model_id LIKE '%mistral%'
    OR model_id LIKE '%mixtral%'
    OR model_id LIKE '%gemma%'
    OR model_id LIKE '%inkling%'
    OR model_id LIKE '%mimo%'
    OR model_id LIKE '%muse-spark%'
  )
  AND NOT (
    model_id LIKE '%whisper%'
    OR model_id LIKE '%tts%'
    OR model_id LIKE '%dall-e%'
    OR model_id LIKE '%imagen%'
    OR model_id LIKE '%recraft%'
    OR model_id LIKE '%flux%'
  );
