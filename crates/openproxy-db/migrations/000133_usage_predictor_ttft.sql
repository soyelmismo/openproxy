-- Add the predictor index through a new migration so existing databases get
-- the same index as fresh installs. Never rewrite migration 000109.
CREATE INDEX IF NOT EXISTS idx_usage_predictor_ttft
  ON usage(provider_id, model_row_id, created_at)
  WHERE status_code >= 200 AND status_code < 400 AND ttft_ms > 0;
