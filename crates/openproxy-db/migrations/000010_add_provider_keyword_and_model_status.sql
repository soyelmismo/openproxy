-- 000010_add_provider_keyword_and_model_status.sql
-- Adds three new columns that drive the dashboard's model activation and
-- test workflows:
--
-- * providers.auto_activate_keyword — when set, the refresh path
--   (`POST /v1/admin/providers/:id/refresh` and the equivalent row-level
--   endpoint) sets each *non-custom* model's `active` bit to whether its
--   `model_id` contains the keyword. When NULL (the default for every
--   existing row), the refresh leaves all discovered models active.
--
-- * models.last_test_status — the most recent HTTP status code from
--   `POST /v1/admin/models/:id/test`. `0` is reserved for "network
--   error" (request never reached the upstream); `NULL` means the model
--   has never been tested.
--
-- * models.last_test_at — companion to `last_test_status`, a sqlite
--   datetime string set at the same time.
--
-- * models.custom — marks rows that were hand-created via
--   `POST /v1/admin/models/custom` (not produced by an adapter's
--   `/models` discovery). The auto-activation logic skips these so an
--   operator's hand-picked entries survive a refresh.
ALTER TABLE providers ADD COLUMN auto_activate_keyword TEXT;

ALTER TABLE models ADD COLUMN last_test_status INTEGER;
ALTER TABLE models ADD COLUMN last_test_at TEXT;
ALTER TABLE models ADD COLUMN custom INTEGER NOT NULL DEFAULT 0
  CHECK (custom IN (0, 1));
