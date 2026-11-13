-- Durable per-request credit intents (see `db::reservations` and
-- `pipeline::credits_gate`).
--
-- A row is written and committed at admission, before anything is dispatched,
-- and closed by the settlement of that same request. The per-key interprocess
-- lock (`credits_gate::interprocess`) guarantees that at most one live process
-- owns an *open* intent for a key, so every `held` row observed while the lock
-- is held belongs to a request whose owner is gone (settled elsewhere,
-- cancelled or crashed). Admission reconciles those rows before it reads the
-- balance, and only from evidence that is already durable:
--
--   * billable usage row (2xx, not a race loser) already debited
--                                            -> close `settled`, no second debit
--   * billable usage row not yet debited     -> post the idempotent debit
--                                               (`mark_winner_usage_row`), close
--   * no usage row, incremental checkpoint   -> charge the checkpointed amount,
--     recorded before the last delivered chunk   clamped to the live balance,
--                                               close `settled` (partial)
--   * no usage row, no checkpoint            -> close `released`, nothing charged
--
-- An intent is never converted into a charge on its own and the quoted maximum
-- is never charged for a partial stream: no evidence of produced output means no
-- invented charge. Ambiguous crashes therefore close with nothing billed (the
-- pre-existing contract: consumption that was not delivered is not billable),
-- which is recorded in `outcome`/`note`.
--
-- `reserved_units` is the budget snapshot the request was admitted under
-- (`quoted = 1` when the caller supplied an explicit maximum-cost quote instead
-- of the whole balance); `generated_units` is the highest durable checkpoint the
-- worker reported before a client-visible chunk. The ledger lots are never
-- touched by a hold, so the unused remainder needs no refund and keeps its
-- original expiry.
--
-- Ownership is immutable: retrying the same `(request_id, attempt)` may refresh
-- the budget of an *open* intent of the same key, but it never re-arms a closed
-- intent and never moves it to another key — evidence cannot be reset.
CREATE TABLE credit_reservations (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    api_key_id        INTEGER NOT NULL,
    request_id        TEXT    NOT NULL,                  -- request id (chat) or generated intent id (unary)
    attempt           INTEGER NOT NULL DEFAULT 1,
    reserved_units    INTEGER NOT NULL,                  -- budget snapshot at admission
    quoted            INTEGER NOT NULL DEFAULT 0,        -- 1 = caller supplied an explicit max-cost quote
    state             TEXT    NOT NULL DEFAULT 'held',   -- held | settled | released
    delivery_started  INTEGER NOT NULL DEFAULT 0,        -- first client-visible chunk/response was sent
    delivery_started_at TEXT,
    generated_units   INTEGER NOT NULL DEFAULT 0,        -- highest durable checkpoint before a delivered chunk
    generated_at      TEXT,
    outcome           TEXT,                              -- billed | unbilled | recovered_billed
                                                         -- | recovered_partial | recovered_unbilled
    charged_units     INTEGER NOT NULL DEFAULT 0,        -- units the close actually accounted
    note              TEXT,                              -- human audit trail of the close
    created_at        TEXT    NOT NULL DEFAULT (datetime('now')),
    closed_at         TEXT,
    UNIQUE(request_id, attempt)
);

CREATE INDEX idx_credit_reservations_open
    ON credit_reservations(api_key_id, state);
