-- 000074_notif_keyword_only.sql
-- Add a per-provider `notif_keyword_only` flag that restricts auto-activation
-- notifications to only the keyword-matched model(s) when a keyword is set.
-- Semantics (see crates/openproxy-db/src/models.rs::apply_auto_activation):
--   * DEFAULT 0  -> normal behaviour: every newly-active model is notified.
--   * = 1 AND keyword=Some(k) -> only the LIKE-matched newly-active model(s)
--     are candidate for the model_auto_activated notification.
--   * = 1 AND keyword=None    -> no-op (normal behaviour; documented).
-- Stored as INTEGER 0/1 so it reads as a boolean from SQLite and satisfies the
-- NOT NULL DEFAULT 0 CHECK constraint.

ALTER TABLE providers ADD COLUMN notif_keyword_only INTEGER NOT NULL DEFAULT 0 CHECK(notif_keyword_only IN (0, 1));
