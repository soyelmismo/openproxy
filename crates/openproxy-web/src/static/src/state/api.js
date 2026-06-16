// state/api.js — re-exports the `api()` helper so handlers can
// `import { api } from "../state/api.js"` without reaching into
// the lib/ tree. Also adds a tiny latency tracker that updates
// `state.lastApiLatencyMs` on every call (the sidebar uses it).

import { state } from "./index.js";

export async function api(path, opts = {}) {
  const t0 = performance.now();
  const init = { method: opts.method || "GET", headers: { "Content-Type": "application/json" } };
  if (opts.body) init.body = opts.body;
  const r = await fetch("/web/api" + path, init);
  if (!r.ok) {
    const txt = await r.text();
    throw new Error(`${r.status}: ${txt}`);
  }
  if (r.status === 204) return null;
  const ct = r.headers.get("content-type") || "";
  const data = ct.includes("application/json") ? await r.json() : await r.text();
  state.lastApiLatencyMs = performance.now() - t0;
  return data;
}
