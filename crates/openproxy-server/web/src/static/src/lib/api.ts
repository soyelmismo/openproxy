// lib/api.ts — thin fetch wrapper. Throws `new Error("<status>: <body>")`
// on non-2xx so call sites can pull a human message out with
// `extractApiErrorMessage(e)`.
//
// Post-F0 (single-binary merge): the dashboard talks DIRECTLY to the
// openproxy server's `/admin/api/*` surface — same origin, no proxy.
// The previous `/web/api/*` prefix (which the now-removed separate
// dashboard binary reverse-proxied to `/admin/*` on the core server)
// is gone.
//
// DASHBOARD-FIX (Bug 2): every request now carries an
// `Authorization: Bearer <token>` header sourced from
// `state/auth.ts::getToken()`. If the user is not logged in (no
// token), the header is omitted — the server then returns 401, the
// caller throws, and the router's auth gate redirects to the login
// view. This is intentional: the only path that should reach the
// server without a token is the login view's own validation call
// (which sets the token optimistically before the call).

import { state } from "../state/index.js";
import { getToken } from "../state/auth.js";
import type { DebugLogsResponse } from "./types/api.js";

export interface ApiOptions {
  method?: string;
  body?: string;
}

export async function api(path: string, opts: ApiOptions = {}): Promise<unknown> {
  const t0: number = performance.now();
  // Build the headers as a mutable record so we can conditionally
  // attach the Authorization header. The cast through
  // `Record<string, string>` is necessary because `RequestInit.headers`
  // is typed as `HeadersInit` (which includes `Headers` and
  // `[string, string][]`), and TS won't let us index into the union.
  const headers: Record<string, string> = { "Content-Type": "application/json" };
  const token: string | null = getToken();
  if (token) {
    headers["Authorization"] = `Bearer ${token}`;
  }
  const init: RequestInit = { method: opts.method || "GET", headers };
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

// Returns the latency in ms for the last `api()` call. Used by the
// health pill in the sidebar.
export function lastApiLatency(): number { return state.lastApiLatencyMs; }

// ----------------------------------------------------------------------------
// Debug logs — typed wrappers around `GET /admin/api/debug/logs` and
// `POST /admin/api/debug/clear`. The view (`views/debug-logs.ts`) calls
// these instead of `api()` directly so the response shape is checked
// at compile time and the query-string construction is centralised.
// ----------------------------------------------------------------------------

/** Optional query parameters for `fetchDebugLogs`. Every field is
 *  optional; absent fields are omitted from the query string (not
 *  sent as `undefined`). Under `exactOptionalPropertyTypes` callers
 *  must build the opts object conditionally — see `views/debug-logs.ts`
 *  for the pattern. */
export interface FetchDebugLogsOpts {
  /** If set, only return entries with `seq > since`. Used by the
   *  polling loop to fetch only new entries. Omit (or pass 0) to
   *  fetch the whole buffer. */
  since?: number;
  /** Cap on the number of entries returned. Server default 100,
   *  server max 1000. */
  limit?: number;
  /** Comma-separated list of levels (e.g. `"WARN,ERROR"`). The
   *  server splits on `,`, uppercases, and matches case-insensitively. */
  level?: string;
  /** Filter by `request_id` (exact match). */
  request_id?: string;
  /** Filter by `trace_id` (exact match). */
  trace_id?: string;
}

/** `GET /admin/api/debug/logs` — fetch recent `tracing` events from
 *  the server's in-memory ring buffer. The dashboard talks directly
 *  to the server's `/admin/api/*` surface (post-F0 single-binary
 *  merge), so the path passed to `api()` is `/debug/logs` and the
 *  `/admin/api` prefix is prepended by `api()` itself. */
export async function fetchDebugLogs(opts: FetchDebugLogsOpts = {}): Promise<DebugLogsResponse> {
  const params = new URLSearchParams();
  if (opts.since !== undefined) params.set("since", String(opts.since));
  if (opts.limit !== undefined) params.set("limit", String(opts.limit));
  if (opts.level) params.set("level", opts.level);
  if (opts.request_id) params.set("request_id", opts.request_id);
  if (opts.trace_id) params.set("trace_id", opts.trace_id);
  const qs: string = params.toString();
  const path: string = qs ? `/debug/logs?${qs}` : "/debug/logs";
  const data: unknown = await api(path);
  // The server always returns a JSON object on 2xx; the `api()`
  // wrapper already parsed it. We cast through `unknown` to the
  // typed shape — a runtime type-guard would be more defensive but
  // the contract is stable and a bad payload is a server bug.
  return data as DebugLogsResponse;
}

/** `POST /admin/api/debug/clear` — wipe the in-memory debug log ring
 *  buffer on the server. Used by the "Clear" button in the Debug
 *  Logs view for "reproduce then capture" workflows. Returns void;
 *  errors propagate as `Error("<status>: <body>")` from `api()`. */
export async function clearDebugLogs(): Promise<void> {
  // The server returns `{"cleared": true}` (JSON); we discard it.
  await api("/debug/clear", { method: "POST" });
}
