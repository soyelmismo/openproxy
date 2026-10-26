// lib/api.ts — thin fetch wrapper. Throws `new Error("<status>: <body>")` on non-2xx so call
// sites can pull a human message out with `extractApiErrorMessage(e)`.
//
// Post-F0 (single-binary merge) the dashboard talks DIRECTLY to the server's `/admin/api/*`
// surface — same origin, no proxy. The old `/web/api/*` prefix is gone.
//
// DASHBOARD-FIX (Bug 2): every request carries `Authorization: Bearer <token>` from
// `state/auth.ts::getToken()`. When there is no token the header is omitted, the server returns
// 401, the caller throws and the router's auth gate redirects to the login view. That is
// intentional: the only tokenless path should be the login view's own validation call, which
// sets the token optimistically before the call.

import { state } from "../state/index.js";
import { getToken, clearToken } from "../state/auth.js";
import type { DebugLogsResponse } from "./types/api.js";
import type { FormatsMetadata } from "./types/common.js";
import { FALLBACK_TARGET_FORMATS, FALLBACK_PROVIDER_FORMATS } from "./types/common.js";

export interface ApiOptions {
  method?: string;
  body?: string;
}

export async function api(path: string, opts: ApiOptions = {}): Promise<unknown> {
  const t0: number = performance.now();
  const token: string | null = getToken();
  const headers: HeadersInit = token
    ? { "Content-Type": "application/json", Authorization: `Bearer ${token}` }
    : { "Content-Type": "application/json" };
  const init: RequestInit = { method: opts.method || "GET", headers };
  if (opts.body) init.body = opts.body;
  const r: Response = await fetch("/admin/api" + path, init);

  if (!r.ok) {
    if (r.status === 401) {
      clearToken();
      if (typeof location !== "undefined" && !location.hash.startsWith("#/login") && path !== "/notifications/unread-count") {
        location.hash = "#/login";
      }
    }
    const txt: string = await r.text();
    throw new Error(`${r.status}: ${txt}`);
  }
  // 204 No Content (e.g. DELETE success with an empty body)
  if (r.status === 204) return null;
  const ct: string = r.headers.get("content-type") || "";
  const data: unknown = ct.includes("application/json") ? await r.json() : await r.text();
  state.lastApiLatencyMs = performance.now() - t0;
  return data;
}

// Latency in ms of the last `api()` call. Used by the sidebar health pill.
export function lastApiLatency(): number { return state.lastApiLatencyMs; }

// ── Debug logs — typed wrappers around GET /admin/api/debug/logs and POST ──
// /admin/api/debug/clear ─────────────────────────────────────────────────────
// views/debug-logs.ts calls these instead of `api()` so the response shape is checked at
// compile time and the query-string construction is centralised.

/** Optional query parameters for `fetchDebugLogs`; every field is optional and absent fields
 *  are omitted from the query string (never sent as `undefined`). Under
 *  `exactOptionalPropertyTypes` callers must build the opts object conditionally — see
 *  `views/debug-logs.ts`. */
export interface FetchDebugLogsOpts {
  /** Only return entries with `seq > since`; the polling loop fetches just the new ones.
   *  Omit (or pass 0) to fetch the whole buffer. */
  since?: number;
  /** Cap on entries returned. Server default 100, server max 1000. */
  limit?: number;
  /** Comma-separated levels (e.g. `"WARN,ERROR"`); the server splits on `,`, uppercases
   *  and matches case-insensitively. */
  level?: string;
  /** Filter by `request_id` (exact). */
  request_id?: string;
  /** Filter by `trace_id` (exact). */
  trace_id?: string;
}

/** `GET /admin/api/debug/logs` — recent `tracing` events from the server's in-memory ring
 *  buffer. The dashboard talks directly to `/admin/api/*` (post-F0 single-binary merge), so the
 *  path passed to `api()` is `/debug/logs` and the `/admin/api` prefix is prepended by `api()`. */
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
  // The server always returns a JSON object on 2xx and `api()` already parsed it; cast through
  // `unknown` to the typed shape — a runtime type-guard would be more defensive but the
  // contract is stable and a bad payload is a server bug.
  return data as DebugLogsResponse;
}

/** `POST /admin/api/debug/clear` — wipe the in-memory debug ring buffer, backing the Debug
 *  Logs view's "Clear" button for reproduce-then-capture workflows. Returns void; errors
 *  propagate as `Error("<status>: <body>")` from `api()`. */
export async function clearDebugLogs(): Promise<void> {
  // The server returns `{"cleared": true}`; discarded.
  await api("/debug/clear", { method: "POST" });
}

/** `GET /admin/api/formats` — fetch introspectable target wire formats and provider formats
 *  auto-indexed by the backend engine as single source of truth. */
export async function fetchFormats(): Promise<FormatsMetadata> {
  if (state.formats) return state.formats;
  try {
    const data = (await api("/formats")) as FormatsMetadata;
    if (data && Array.isArray(data.target_formats) && Array.isArray(data.provider_formats)) {
      state.formats = data;
      return data;
    }
  } catch (err: unknown) {
    console.error("Failed to fetch formats metadata from /admin/api/formats:", err);
  }
  return {
    target_formats: [...FALLBACK_TARGET_FORMATS],
    provider_formats: [...FALLBACK_PROVIDER_FORMATS],
  };
}
