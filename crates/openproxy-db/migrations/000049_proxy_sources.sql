CREATE TABLE proxy_sources (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    url TEXT NOT NULL,
    priority INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

ALTER TABLE free_proxies ADD COLUMN username TEXT;
ALTER TABLE free_proxies ADD COLUMN password TEXT;
ALTER TABLE free_proxies ADD COLUMN priority INTEGER NOT NULL DEFAULT 0;
