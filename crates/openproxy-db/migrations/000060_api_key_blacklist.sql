-- 000060_api_key_blacklist.sql
-- Add blacklisted_providers_json and blacklisted_models_json to api_keys table

ALTER TABLE api_keys ADD COLUMN blacklisted_providers_json TEXT;
ALTER TABLE api_keys ADD COLUMN blacklisted_models_json TEXT;
