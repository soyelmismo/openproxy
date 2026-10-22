-- 000095_provider_prompt_cache_ttl.sql
-- Add prompt_cache_ttl column to providers table:
-- 'auto': default provider-native caching behavior (no explicit override)
-- OpenAI / Codex (Responses API): 'auto', '5m', '1h', '12h', '24h', etc.
-- Anthropic / Claude (Messages API cache_control.ttl): 'auto', '5m', '1h', etc.
ALTER TABLE providers ADD COLUMN prompt_cache_ttl TEXT NOT NULL DEFAULT 'auto';
