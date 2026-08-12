-- 000054_add_cached_tokens.sql

-- API Cache Hits metric, to track upstream context caching (e.g. Anthropic Prompt Caching, Gemini caching).
ALTER TABLE usage ADD COLUMN cached_tokens INTEGER;
