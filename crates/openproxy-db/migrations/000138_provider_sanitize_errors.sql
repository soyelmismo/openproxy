-- 000138_provider_sanitize_errors.sql
--
-- Per-provider upstream-error masking switch. Default ON (1): any 4xx/5xx the
-- target answers is reduced to a generic class before it reaches the client
-- (same classes the combo-level error masking produces), so buyers never see
-- the upstream's provider name, account detail or retry windows. The real
-- cause stays in usage, live logs and traces.
--
-- Why per-provider and not only the global combo masking: direct model routes
-- (combo_id = -1, e.g. the antseed seller driving a provider by name) are
-- deliberately exempt from the combo masking because their caller is usually
-- the operator debugging that provider. When the caller is a reseller's
-- downstream client instead, hiding the upstream detail belongs to the
-- provider, not to the combo.

ALTER TABLE providers ADD COLUMN sanitize_errors INTEGER NOT NULL DEFAULT 1;
