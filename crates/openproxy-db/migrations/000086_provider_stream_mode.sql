-- 000086_provider_stream_mode.sql
-- Add stream_mode column to providers table:
-- 'auto': default behavior (upstream follows client / pipeline streaming)
-- 'streaming': force upstream streaming
-- 'unary': force upstream non-streaming (unary)
ALTER TABLE providers ADD COLUMN stream_mode TEXT NOT NULL DEFAULT 'auto' CHECK(stream_mode IN ('auto', 'streaming', 'unary'));
