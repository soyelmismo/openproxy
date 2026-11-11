-- Inter-chunk gap telemetry for streaming bodies. `UpstreamBodyStream`
-- measures the gaps between content/progress chunks it survives and the
-- usage tracker records the largest (and mean) per completed stream.
-- Feeds the learned idle-chunk budget predictor
-- (`openproxy-pipeline::latency_budget::chunk_budget`): gaps a stream
-- actually survived are the honest sample of what `idle_chunk` needs.
-- NULL for unary requests and single-chunk streams.
ALTER TABLE usage ADD COLUMN max_chunk_gap_ms INTEGER;
ALTER TABLE usage ADD COLUMN avg_chunk_gap_ms INTEGER;

CREATE INDEX idx_usage_chunk_gap
  ON usage(provider_id, model_row_id, created_at)
  WHERE max_chunk_gap_ms IS NOT NULL;
