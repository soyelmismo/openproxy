-- 000014_add_model_metadata.sql
-- Adds capability + metadata columns to the `models` table so the public
-- `GET /v1/models` endpoint can hand clients like Cursor and Cline the
-- context window, vision/tool/reasoning flags, and modalities they need
-- to auto-detect model behavior.
--
-- All columns are nullable except `model_type`, which defaults to
-- 'chat' (the only kind of model the existing adapters can produce).
-- The endpoint falls back to a heuristic that derives these fields
-- from the model_id when the column is NULL, so existing rows do not
-- need to be backfilled for the endpoint to work — the heuristic
-- covers OpenAI/Claude/Gemini/Llama/etc. out of the box.

ALTER TABLE models ADD COLUMN context_length INTEGER;
ALTER TABLE models ADD COLUMN max_output_tokens INTEGER;
ALTER TABLE models ADD COLUMN capabilities_json TEXT;
ALTER TABLE models ADD COLUMN family TEXT;
ALTER TABLE models ADD COLUMN model_type TEXT NOT NULL DEFAULT 'chat';
ALTER TABLE models ADD COLUMN input_modalities_json TEXT;
ALTER TABLE models ADD COLUMN output_modalities_json TEXT;
