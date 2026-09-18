-- 000077_provider_favicons.sql
-- Dedicated on-disk binary storage for provider favicons,
-- evicting base64 text from in-memory Provider rows.

CREATE TABLE IF NOT EXISTS provider_favicons (
    provider_id TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE,
    mime        TEXT NOT NULL,
    data        BLOB NOT NULL,
    updated_at  INTEGER NOT NULL
);
