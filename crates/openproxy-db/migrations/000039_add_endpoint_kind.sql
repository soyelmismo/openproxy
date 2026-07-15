-- 000039_add_endpoint_kind.sql
-- Adds endpoint_kind to usage table so we can distinguish chat, audio,
-- image, embedding, video requests. Existing rows default to 'chat'.
ALTER TABLE usage ADD COLUMN endpoint_kind TEXT NOT NULL DEFAULT 'chat';
