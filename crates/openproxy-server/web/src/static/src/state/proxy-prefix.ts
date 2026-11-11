// Reverse-proxy prefix detection — the ONE place the SPA learns where it is
// mounted. nginx (or any front proxy) may serve the whole user surface under
// a prefixed path (e.g. /tokens/...) while the backend only ever knows its
// own internal routes (/admin, /checkout, /v1). The prefix lives in nginx
// config alone; this module derives it from the URL at runtime.
//
// Rules: the first path segment is the proxy prefix UNLESS it is one of the
// backend's own mount points (today: "admin"). Served at /admin → no prefix;
// served at /tokens/admin → prefix "/tokens".
const INTERNAL_MOUNTS: ReadonlySet<string> = new Set(["admin", "checkout", "v1"]);

export function detectProxyPrefix(): string {
  if (typeof location === "undefined") return "";
  const seg: string = (location.pathname ?? "").split("/")[1] ?? "";
  return seg && !INTERNAL_MOUNTS.has(seg) ? "/" + seg : "";
}

/** Proxy prefix as a string, "" when mounted at the backend root. */
export const PROXY_PREFIX: string = detectProxyPrefix();
