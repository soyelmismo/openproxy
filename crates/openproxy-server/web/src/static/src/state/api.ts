// state/api.ts — re-exports the `api()` helper so handlers can
// `import { api } from "../state/api.js"` without reaching into
// the lib/ tree. Also adds a tiny latency tracker that updates
// `state.lastApiLatencyMs` on every call (the sidebar uses it).
//
// Coexists with `state/api.js`: tsc + Bundler resolution picks the
// `.ts` for the .ts importers, and the `.js` is still used by the
// not-yet-migrated views/handlers/components. They expose the same
// runtime shape, so the dual import resolves cleanly.
//
// DASHBOARD-FIX (Bug 2): every request now carries an
// `Authorization: Bearer <token>` header sourced from
// `state/auth.ts::getToken()`. This is the `api()` used by every
// view/handler/component EXCEPT `state/live-store.ts` and
// `views/debug-logs.ts` (which import from `lib/api.ts` — same
// fix applied there). Without this patch, the 401s the user saw
// (`fetchRecordingState failed`, `live-store rehydrate failed`,
// every Keys / Combos / Providers fetch) would persist.

import { state } from "./index.js";
import { getToken } from "./auth.js";

export interface ApiCallOptions {
  method?: string;
  body?: string;
}

export async function api(path: string, opts: ApiCallOptions = {}): Promise<unknown> {
  const t0: number = performance.now();
  const headers: Record<string, string> = { "Content-Type": "application/json" };
  const token: string | null = getToken();
  if (token) {
    headers["Authorization"] = `Bearer ${token}`;
  }
  const init: RequestInit = {
    method: opts.method || "GET",
    headers,
  };
  if (opts.body) init.body = opts.body;
  const r: Response = await fetch("/admin/api" + path, init);
  if (!r.ok) {
    const txt: string = await r.text();
    throw new Error(`${r.status}: ${txt}`);
  }
  // 204 No Content (e.g. DELETE success with empty body)
  if (r.status === 204) return null;
  const ct: string = r.headers.get("content-type") || "";
  const data: unknown = ct.includes("application/json") ? await r.json() : await r.text();
  state.lastApiLatencyMs = performance.now() - t0;
  return data;
}
