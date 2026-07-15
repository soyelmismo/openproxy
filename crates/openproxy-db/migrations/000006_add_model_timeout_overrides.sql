-- 000006_add_model_timeout_overrides.sql
ALTER TABLE models ADD COLUMN timeout_overrides_json TEXT;
