// lib/api.js — thin fetch wrapper. Throws `new Error("<status>: <body>")`
// on non-2xx so call sites can pull a human message out with
// `extractApiErrorMessage(e)`.

import { state } from "../state/index.js";

export async function api(path, opts = {}) {
  const init = { method: opts.method || "GET", headers: { "Content-Type": "application/json" } };
  if (opts.body) init.body = opts.body;
  const r = await fetch("/web/api" + path, init);
  if (!r.ok) {
    const txt = await r.text();
    throw new Error(`${r.status}: ${txt}`);
  }
  // 204 No Content (e.g. DELETE success with empty body)
  if (r.status === 204) return null;
  const ct = r.headers.get("content-type") || "";
  if (ct.includes("application/json")) return r.json();
  return r.text();
}

// Returns the latency in ms for the last `api()` call. Used by the
// health pill in the sidebar.
export function lastApiLatency() { return state.lastApiLatencyMs; }
