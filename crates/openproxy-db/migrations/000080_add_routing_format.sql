-- 000080_add_routing_format.sql
-- Add routing_format column to model_capabilities_sync table to dynamically
-- track upstream wire target format (openai, anthropic, gemini, responses) from models.dev

ALTER TABLE model_capabilities_sync ADD COLUMN routing_format TEXT;
