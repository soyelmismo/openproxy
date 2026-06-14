-- 000008_add_error_msg_redacted.sql
-- See §8 for redaction policy and §6/§9 for the error capture contract.
ALTER TABLE usage ADD COLUMN error_msg_redacted TEXT;
