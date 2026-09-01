-- Per-(account, model) "live-limited" sentinel.
--
-- When a target hits a per-model rate limit (e.g. antigravity
-- 429 RESOURCE_EXHAUSTED on a specific model while the rest of
-- the account is healthy), the dispatcher records a row here
-- with an `until_ts` TTL. The quota sync daemon clears all rows
-- for an account after a successful refresh, on the assumption
-- that "if the account is back, all its models are back too".
--
-- ON DELETE CASCADE on account_id ensures removing an account
-- also drops its live-limit rows.
--
-- See docs/specs/antigravity-gaps-p2.md §4 (GAP-6).
CREATE TABLE live_limited_models (
    account_id INTEGER NOT NULL,
    model_id   TEXT    NOT NULL,
    until_ts   TEXT    NOT NULL,
    reason     TEXT    NOT NULL DEFAULT 'RESOURCE_EXHAUSTED',
    PRIMARY KEY (account_id, model_id),
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE
);
CREATE INDEX idx_live_limited_until ON live_limited_models(until_ts);