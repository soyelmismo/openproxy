-- Credit packages: admin-defined presets. Buying one copies its values onto
-- the buyer's key (price, balance lifetime, quota caps, model/provider/combo
-- allowlists) — copy and paste, no manual assignment.
-- `config_json` holds the key-shaping payload verbatim:
--   { monthly_usd_cap, weekly_usd_cap, five_hour_usd_cap,
--     allowed_models, allowed_providers, allowed_combos, only_whitelisted }
-- NULL JSON fields mean "leave whatever the key already has".

CREATE TABLE credit_packages (
    id          INTEGER PRIMARY KEY,
    name        TEXT    NOT NULL UNIQUE,
    price_usd   REAL    NOT NULL CHECK (price_usd > 0),
    ttl_days    INTEGER NOT NULL CHECK (ttl_days > 0),
    config_json TEXT,
    active      INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
    created_at  TEXT    NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT    NOT NULL DEFAULT (datetime('now'))
);

ALTER TABLE credit_orders ADD COLUMN package_id INTEGER REFERENCES credit_packages(id);
CREATE INDEX idx_credit_packages_active ON credit_packages(active);
