-- Notifications tray: surfaces discovery + system events to dashboard users.
-- Designed to be polymorphic via `kind` + `payload_json` so new notification
-- types can be added without schema changes (only the CHECK constraint list
-- needs updating).
CREATE TABLE notifications (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    -- Notification type. Adding a new type = bump this CHECK + add a constant
    -- in notifications.rs. Existing rows are untouched.
    kind            TEXT NOT NULL CHECK (kind IN (
                        'model_new',            -- newly discovered model inserted
                        'model_gone',           -- model hard-deleted from upstream
                        'model_auto_activated', -- model matched auto_activate_keyword
                        'system'                -- generic: scheduler errors, oauth expiry, circuit opens, etc.
                    )),
    -- JSON payload. Schema depends on `kind`. See notifications.rs for the
    -- per-kind structs. Stored as TEXT (SQLite has no native JSON type).
    payload_json    TEXT NOT NULL,
    -- Read state: NULL = unread, ISO8601 timestamp = when user dismissed/read.
    read_at         TEXT,
    -- Lifecycle: NULL = active (shown in tray), ISO8601 = archived (hidden from
    -- tray, row kept for audit). Archived rows can be purged by a future GC job.
    archived_at     TEXT,
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    -- De-duplication: same (kind, dedup_key, date) collapses into one row.
    -- dedup_key for model_* is "provider_id:model_id". For system, it's a
    -- stable hash of the message. NULL allows unlimited duplicates.
    dedup_key       TEXT,
    -- Soft reference to the provider (NOT a FK — models can be hard-deleted,
    -- and we still want the notification to reference which provider it came
    -- from for "go to provider" navigation in the dashboard).
    provider_id     TEXT
);

-- Hot path: dashboard fetches unread count + recent unread list.
CREATE INDEX idx_notifications_unread
    ON notifications(read_at, created_at DESC)
    WHERE read_at IS NULL;

-- Recent activity view (read + unread, active + archived).
CREATE INDEX idx_notifications_recent
    ON notifications(created_at DESC);

-- De-dup UPSERT target. The date(...) function collapses all rows created on
-- the same UTC day into one dedup window — prevents the same model_new from
-- being inserted twice if discovery runs twice in 24h.
CREATE UNIQUE INDEX idx_notifications_dedup
    ON notifications(kind, dedup_key, date(created_at))
    WHERE dedup_key IS NOT NULL;
