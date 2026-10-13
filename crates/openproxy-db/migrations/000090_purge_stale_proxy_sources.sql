-- 000090_purge_stale_proxy_sources.sql
-- Purges dead and abandoned proxy sources from database and cleans up references.

DELETE FROM free_proxies WHERE source IN ('hookzof', 'yuceltoluyag', 'komutan234', '1proxy');
DELETE FROM proxy_sources WHERE id = 'builtin_oneproxy';
