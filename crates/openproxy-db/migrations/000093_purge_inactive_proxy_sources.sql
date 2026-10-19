-- 000093_purge_inactive_proxy_sources.sql
-- Purges scraped proxies belonging to deactivated or deleted proxy sources.

DELETE FROM free_proxies 
WHERE source NOT IN ('custom', 'warp')
  AND (
    (source IN ('proxifly') AND EXISTS(SELECT 1 FROM proxy_sources WHERE id = 'builtin_proxifly' AND active = 0))
    OR (source IN ('iplocate', 'hideip', 'r00tee', 'anonymouswork', 'kael_odin', 'thespeedx') AND EXISTS(SELECT 1 FROM proxy_sources WHERE id = 'builtin_github' AND active = 0))
    OR (source IN ('proxmint') AND EXISTS(SELECT 1 FROM proxy_sources WHERE id = 'builtin_proxmint' AND active = 0))
    OR (source IN ('proxyscrape_cdn') AND EXISTS(SELECT 1 FROM proxy_sources WHERE id = 'builtin_proxyscrape' AND active = 0))
    OR (source IN ('geonode') AND EXISTS(SELECT 1 FROM proxy_sources WHERE id = 'builtin_geonode' AND active = 0))
    OR (source IN ('clearproxy') AND EXISTS(SELECT 1 FROM proxy_sources WHERE id = 'builtin_clearproxy' AND active = 0))
    OR (source IN ('vakhov') AND EXISTS(SELECT 1 FROM proxy_sources WHERE id = 'builtin_vakhov' AND active = 0))
    OR (source IN ('gproxynet') AND EXISTS(SELECT 1 FROM proxy_sources WHERE id = 'builtin_gproxynet' AND active = 0))
    OR source IN (SELECT name FROM proxy_sources WHERE active = 0 AND is_builtin = 0)
  );
