-- The fixed 5h quota window anchors to the key's first request, not to a
-- global clock: `five_hour_started_at` is the moment that window opened and
-- its whole cap frees at anchor + 5h. NULL means no window is running (the
-- key has sent nothing, or the last window expired and will re-anchor with
-- the next request). Payments-only: the quota table it extends is 000110.
ALTER TABLE api_key_quotas ADD COLUMN five_hour_started_at TEXT;
