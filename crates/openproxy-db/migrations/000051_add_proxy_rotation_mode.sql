ALTER TABLE providers ADD COLUMN proxy_rotation_mode TEXT NOT NULL DEFAULT 'global';
ALTER TABLE accounts ADD COLUMN current_proxy_id TEXT;
