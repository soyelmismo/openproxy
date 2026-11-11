-- Credits: prepaid balance with expiry, funded by on-chain USDC top-ups
-- on a single collector address (thin funnel).
--
-- Money model (all amounts in integer micro-USDC units, 1e-6 USD — no floats
-- inside the ledger):
--   credit_orders        one per checkout. `amount_units` carries the exact
--                        micro-amount the buyer must send; its low 6 digits
--                        are the jitter that makes the amount a unique
--                        identifier among live orders.
--   matched_payments     on-chain Transfer logs already consumed by the
--                        watcher. Primary key (tx_hash, log_index) makes
--                        processing idempotent across restarts and re-scans.
--   credit_transactions  the ledger. `topup` rows carry `remaining_units` and
--                        `expires_at` (paid_at + ttl_days). `spend` and
--                        `expire` rows adjust balances. Balance of a key =
--                        SUM(remaining_units) over its active rows.
--   credits_watcher_state last scanned block for the single watcher task.

CREATE TABLE credit_orders (
    id            INTEGER PRIMARY KEY,
    order_code    TEXT    NOT NULL UNIQUE,          -- OP-XXXX-XXXX
    api_key_id    INTEGER,                          -- NULL = anonymous checkout (key minted on payment)
    payer_email   TEXT,                             -- optional, anonymous checkout contact
    amount_units  INTEGER NOT NULL,                 -- exact micro-USDC to send (base + jitter)
    base_units    INTEGER NOT NULL,                 -- micro-USDC credited (no jitter)
    ttl_days      INTEGER NOT NULL,                 -- balance lifetime after payment
    status        TEXT    NOT NULL DEFAULT 'pending', -- pending|paid|minted|expired|cancelled
    tx_hash       TEXT,                             -- pointer into the Base chain
    block_number  INTEGER,
    key_revealed  INTEGER NOT NULL DEFAULT 0,       -- anonymous: plaintext key shown once
    minted_key_encrypted BLOB,                        -- AES-GCM(plaintext) for anonymous orders,
                                                      -- wiped when the key is revealed
    created_at    TEXT    NOT NULL DEFAULT (datetime('now')),
    expires_at    TEXT    NOT NULL,                 -- payment window deadline
    paid_at       TEXT
);

-- Jitter uniqueness among live orders: two concurrent checkouts can never
-- advertise the same amount. The DB is the arbiter; creators retry on
-- constraint violation.
CREATE UNIQUE INDEX idx_credit_orders_live_amount
    ON credit_orders(amount_units)
    WHERE status IN ('pending', 'paid');

CREATE INDEX idx_credit_orders_key ON credit_orders(api_key_id);
CREATE INDEX idx_credit_orders_status ON credit_orders(status, expires_at);

CREATE TABLE matched_payments (
    tx_hash      TEXT    NOT NULL,
    log_index    INTEGER NOT NULL,
    block_number INTEGER NOT NULL,
    amount_units INTEGER NOT NULL,
    from_address TEXT,
    seen_at      TEXT    NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (tx_hash, log_index)
);

CREATE TABLE credit_transactions (
    id              INTEGER PRIMARY KEY,
    api_key_id      INTEGER NOT NULL,
    order_id        INTEGER,                         -- NULL for manual adjustments
    kind            TEXT    NOT NULL,                -- topup|spend|expire|adjust
    amount_units    INTEGER NOT NULL,                -- signed: + topup, - spend/expire
    remaining_units INTEGER NOT NULL DEFAULT 0,      -- topup rows only: what is left of this deposit
    tx_hash         TEXT,                            -- on-chain pointer of the funding payment
    status          TEXT    NOT NULL DEFAULT 'active', -- active|exhausted|expired
    created_at      TEXT    NOT NULL DEFAULT (datetime('now')),
    expires_at      TEXT
);

CREATE INDEX idx_credit_tx_key ON credit_transactions(api_key_id, status, expires_at);
CREATE INDEX idx_credit_tx_order ON credit_transactions(order_id);

CREATE TABLE credits_watcher_state (
    id                 INTEGER PRIMARY KEY CHECK (id = 1),
    last_scanned_block INTEGER NOT NULL DEFAULT 0,
    updated_at         TEXT    NOT NULL DEFAULT (datetime('now'))
);

INSERT INTO credits_watcher_state (id, last_scanned_block) VALUES (1, 0);
