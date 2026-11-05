-- 000106_api_key_allowed_providers.sql
-- Adds `allowed_providers_json` to `api_keys` for provider-level whitelisting.
-- NULL means no provider whitelist restriction.
-- When non-null (e.g. '["xkiro"]'), only models belonging to the whitelisted
-- providers are allowed, inverting the behavior of `blacklisted_providers_json`.
ALTER TABLE api_keys ADD COLUMN allowed_providers_json TEXT DEFAULT NULL;
