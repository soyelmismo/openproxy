-- 000033_add_model_id_normalized.sql
-- Normalized model ID column for matching against models.dev.
-- Populated by the application on insert/update. Indexed for fast
-- lookups during models.dev sync enrichment and pricing lookup.
--
-- The normalization logic lives in the application
-- (model_normalize::normalize_model_id); we can't call it from SQL.
-- Existing rows are left NULL — they'll be populated on the next
-- models.dev sync or model upsert. New rows get the normalized value
-- set by the application.

ALTER TABLE models ADD COLUMN model_id_normalized TEXT;
ALTER TABLE model_capabilities_sync ADD COLUMN model_id_normalized TEXT;

CREATE INDEX IF NOT EXISTS idx_models_normalized
    ON models(model_id_normalized);
CREATE INDEX IF NOT EXISTS idx_sync_normalized
    ON model_capabilities_sync(model_id_normalized);
