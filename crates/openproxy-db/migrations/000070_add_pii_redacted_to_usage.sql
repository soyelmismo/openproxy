-- 000070_add_pii_redacted_to_usage.sql
-- Adds pii_redacted column to usage table to track entity redaction summary.
ALTER TABLE usage ADD COLUMN pii_redacted TEXT;
