-- Session fingerprint (64-bit hash) attached to every usage row so the
-- adaptive TTFT predictor can learn a per-session regime in addition to the
-- per-target global one. NULL for requests without a session identity.
ALTER TABLE usage ADD COLUMN session_hash INTEGER;

CREATE INDEX idx_usage_session
  ON usage(provider_id, model_row_id, session_hash, created_at)
  WHERE session_hash IS NOT NULL;
