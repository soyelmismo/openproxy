-- Separate compact analytics data from potentially large recorded payloads.
-- Tenant-range scans are index-only even when bodies remain in usage.
CREATE INDEX IF NOT EXISTS idx_usage_tenant_analytics ON usage(
 api_key_id, created_at, combo_id, model_row_id, status_code, request_id,
 trace_id, race_lost, prompt_tokens, completion_tokens, cached_tokens,
 cost_usd, error_msg, ttft_ms, total_ms, connect_ms, compression_savings_pct
);
