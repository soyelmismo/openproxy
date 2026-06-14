-- 000023_add_live_log_columns.sql
ALTER TABLE usage ADD COLUMN request_body_json TEXT;
ALTER TABLE usage ADD COLUMN response_body_json TEXT;
ALTER TABLE usage ADD COLUMN request_headers TEXT;
ALTER TABLE usage ADD COLUMN response_headers TEXT;
ALTER TABLE usage ADD COLUMN error_message TEXT;
ALTER TABLE usage ADD COLUMN race_attempts INTEGER NOT NULL DEFAULT 1;
ALTER TABLE usage ADD COLUMN is_streaming INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage ADD COLUMN stream_complete INTEGER NOT NULL DEFAULT 0;