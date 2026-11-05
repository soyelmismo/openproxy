-- Per-combo real model ID override for response sanitization.
--
-- When `sanitize_response` is enabled on a combo, if `real_model_id` is set,
-- the outgoing `model` key in unary responses and streaming chunks is rewritten
-- to this exact string (e.g. "gemini-3.5-flash-lite", "claude-sonnet-4-6")
-- instead of falling back to the requested model string or combo name.
ALTER TABLE combos ADD COLUMN real_model_id TEXT;
