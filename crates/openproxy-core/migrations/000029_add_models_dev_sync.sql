-- 000029_add_models_dev_sync.sql
-- Table for models.dev synced data: pricing, context length, capabilities.
-- Mirrors the models.dev API JSON structure. Each row is keyed by our
-- internal provider_id + model_id (after provider mapping).
-- The sync job fetches https://models.dev/api.json, maps providers, and upserts.

CREATE TABLE IF NOT EXISTS model_capabilities_sync (
    provider_id  TEXT NOT NULL,
    model_id     TEXT NOT NULL,
    context_length        INTEGER,
    max_output_tokens     INTEGER,
    pricing_input_per_1m  REAL,
    pricing_output_per_1m REAL,
    pricing_cached_per_1m REAL,
    tool_call      INTEGER,  -- boolean
    reasoning      INTEGER,
    vision         INTEGER,
    structured_output INTEGER,
    temperature    INTEGER,
    modalities_input  TEXT,  -- JSON array
    modalities_output TEXT,
    family         TEXT,
    open_weights   INTEGER,
    status         TEXT,
    knowledge_cutoff TEXT,
    release_date   TEXT,
    last_updated   TEXT,
    fetched_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    PRIMARY KEY (provider_id, model_id)
);

-- Index for looking up by model_id across providers (used for auto-combo).
CREATE INDEX IF NOT EXISTS idx_model_cap_sync_model_id
    ON model_capabilities_sync(model_id);
