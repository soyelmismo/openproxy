-- Credit tiers: named, reusable key-shaping presets referenced by packages.
-- A tier bundles the allowlists + quota caps a buyer's key receives when a
-- package tied to this tier is paid (applied by the payment matcher, same
-- contract as the package config copy-paste).
-- NULL allowlist fields mean "leave whatever the key already has".
CREATE TABLE credit_tiers (
    id                 INTEGER PRIMARY KEY,
    name               TEXT    NOT NULL UNIQUE,
    models_json        TEXT,        -- allowed model ids (same format as api_keys.allowed_models_json)
    providers_json     TEXT,        -- allowed providers
    combos_json        TEXT,        -- allowed combos
    only_whitelisted   INTEGER,     -- NULL = leave key as-is
    monthly_usd_cap    REAL,
    weekly_usd_cap     REAL,
    five_hour_usd_cap  REAL,
    active             INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
    created_at         TEXT    NOT NULL DEFAULT (datetime('now')),
    updated_at         TEXT    NOT NULL DEFAULT (datetime('now'))
);

-- Leveraged packages: the amount CHARGED on-chain is price_usd; the amount
-- CREDITED to the key's balance is credit_usd (NULL keeps the legacy
-- credit == price behaviour). tier_id points at the key-shaping preset.
ALTER TABLE credit_packages ADD COLUMN credit_usd REAL CHECK (credit_usd IS NULL OR credit_usd > 0);
ALTER TABLE credit_packages ADD COLUMN tier_id INTEGER REFERENCES credit_tiers(id);

-- The order stores both sides of the funnel: amount_units is charged on-chain
-- (base_units + jitter today), credit_units is what the ledger credits when
-- the payment lands (NULL keeps the legacy credit == base_units behaviour).
ALTER TABLE credit_orders ADD COLUMN credit_units INTEGER CHECK (credit_units IS NULL OR credit_units > 0);
