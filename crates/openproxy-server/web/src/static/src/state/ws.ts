// state/ws.ts — WebSocket lifecycle for the live-logs view. The
// singleton guard and the reconnect backoff live here; the
// message routing is in views/logs.js.

import { state } from "./index.js";
import { LOGS_WS_RECONNECT_DELAYS } from "../lib/constants.js";
import type { StageEvent } from "../lib/types/api.js";
import { dispatchWs } from "./ws-bus.js";
import type { WsEnvelope } from "../views/logs.js";
import { getToken } from "./auth.js";

/** Connection status for the live-logs view. */
export type LogsStatus = "connected" | "connecting" | "reconnecting" | "disconnected";

export function logsWsUrl(): string {
  const scheme: "ws:" | "wss:" = location.protocol === "https:" ? "wss:" : "ws:";
  // Post-F0 single-binary merge: the live-logs WebSocket is served
  // directly by the openproxy server at `/admin/ws` (was
  // `/web/api/usage/stream` on the now-removed separate dashboard
  // binary, which reverse-proxied to `/admin/usage/stream` on the
  // core server).
  //
  // DASHBOARD-FIX (Bug 3): the WS upgrade handler
  // (`handlers/admin.rs::usage_stream`) authenticates via
  // `authenticate_admin_ws`, which accepts EITHER an
  // `Authorization: Bearer <token>` header OR a `?token=<key>` query
  // param. Browsers cannot set custom headers on `new WebSocket()`
  // (the WebSocket API only supports the `protocols` arg, not
  // arbitrary headers), so we MUST use the query-param path. Without
  // it the upgrade is rejected with 401 before the WS handshake
  // completes → "Firefox no puede establecer una conexión con el
  // servidor en ws://.../admin/ws".
  //
  // `encodeURIComponent` is important: tokens are opaque strings
  // that may contain `+`, `=`, `/` (base64-ish chars). The server
  // decodes via axum's `Query<T>` deserializer, which percent-decodes
  // for us; the encode/decode round-trip preserves the token.
  const base: string = `${scheme}//${location.host}/admin/ws`;
  const token: string | null = getToken();
  if (token) {
    return `${base}?token=${encodeURIComponent(token)}`;
  }
  return base;
}

export function setLogsStatus(status: LogsStatus): void {
  state.logs.status = status;
  const badge: HTMLElement | null = document.getElementById("logs-connection-status");
  if (!badge) return;
  const labels: Record<LogsStatus, string> = {
    connected: "🟢 connected",
    connecting: "🟡 connecting",
    reconnecting: "🟡 reconnecting",
    disconnected: "🔴 disconnected",
  };
  badge.className = `logs-connection-badge ${status}`;
  badge.textContent = labels[status] || "🔴 disconnected";
}

function clearLogsReconnectTimer(): void {
  if (state.logs.reconnectTimer) {
    clearTimeout(state.logs.reconnectTimer);
    state.logs.reconnectTimer = null;
  }
}

function scheduleLogsReconnect(): void {
  clearLogsReconnectTimer();
  const delays: readonly number[] = LOGS_WS_RECONNECT_DELAYS;
  const idx: number = Math.min(state.logs.reconnectAttempt, delays.length - 1);
  const delay: number = delays[idx] ?? delays[delays.length - 1] ?? 1000;
  state.logs.reconnectAttempt += 1;
  state.logs.reconnectTimer = setTimeout(connectLogsWebSocket, delay);
}

/** Type guard for StageEvent. The server emits a JSON object that
 *  matches this shape; anything else is ignored. Exported so
 *  views/logs.js (G4) can reuse it. */
export function isStageEvent(x: unknown): x is StageEvent {
  if (typeof x !== "object" || x === null) return false;
  const o: Record<string, unknown> = x as Record<string, unknown>;
  if (typeof o["request_id"] !== "string") return false;
  if (typeof o["trace_id"] !== "string") return false;
  if (typeof o["provider_id"] !== "string") return false;
  if (typeof o["upstream_model_id"] !== "string") return false;
  if (typeof o["stage"] !== "string") return false;
  if (typeof o["elapsed_ms"] !== "number") return false;
  if (typeof o["status_code"] !== "number") return false;
  if (typeof o["timestamp"] !== "string") return false;
  // `connect_ms`, `ttft_ms`, `error` are nullable.
  return true;
}

// Connected message handler. Set by views/logs.js during mount.
let messageHandler: ((event: MessageEvent) => void) | null = null;
export function setMessageHandler(fn: ((event: MessageEvent) => void) | null): void {
  messageHandler = fn;
}

export function connectLogsWebSocket(): void {
  clearLogsReconnectTimer();
  if (state.logs.ws) {
    const ready: number = state.logs.ws.readyState;
    if (ready === WebSocket.OPEN) { setLogsStatus("connected"); return; }
    if (ready === WebSocket.CONNECTING) return;
  }
  setLogsStatus(state.logs.reconnectAttempt === 0 ? "connecting" : "reconnecting");
  const ws: WebSocket = new WebSocket(logsWsUrl());
  // Heartbeat: send a ping every 15s. The server responds with a
  // pong. If we don't receive a pong within 30s (2 intervals), we
  // consider the connection dead and force-close it. This detects
  // half-open TCP connections (common when the network changes,
  // laptop sleeps/wakes, or a proxy silently drops the WS) that
  // would otherwise leave the dashboard "connected" but receiving
  // no events — the exact "deja de sincronizarse" symptom.
  let lastPong: number = Date.now();
  const heartbeatHandle: ReturnType<typeof setInterval> = setInterval(() => {
    if (state.logs.ws !== ws) {
      clearInterval(heartbeatHandle);
      return;
    }
    if (ws.readyState !== WebSocket.OPEN) {
      clearInterval(heartbeatHandle);
      return;
    }
    // If we haven't received a pong in 30s, the connection is
    // probably half-open. Force-close it; the close handler will
    // trigger a reconnect.
    if (Date.now() - lastPong > 30_000) {
      console.warn("[openproxy] live-logs WS heartbeat timeout — no pong in 30s, forcing reconnect");
      try { ws.close(); } catch (_e: unknown) { /* already closed */ }
      clearInterval(heartbeatHandle);
      return;
    }
    try {
      ws.send(JSON.stringify({ type: "ping" }));
    } catch (_e: unknown) {
      // Send failed — connection is broken. The close handler
      // will trigger a reconnect.
      clearInterval(heartbeatHandle);
    }
  }, 15_000);

  ws.addEventListener("open", () => {
    if (state.logs.ws !== ws) return;
    state.logs.reconnectAttempt = 0;
    lastPong = Date.now();
    setLogsStatus("connected");
    if (state.logs.lastSeenId > 0) {
      ws.send(JSON.stringify({ type: "subscribe", since_id: state.logs.lastSeenId }));
    }
  });
  ws.addEventListener("message", (event: MessageEvent) => {
    if (state.logs.ws !== ws) return;
    // Track pong responses for the heartbeat. Any message from the
    // server means the connection is alive — not just pongs.
    lastPong = Date.now();
    if (typeof messageHandler === "function") {
      // CRITICAL: wrap the entire handler in try/catch. Without this,
      // a single malformed WS message (e.g. an unexpected null field
      // in a usage row) would throw out of `messageHandler`, leave
      // `state.logs` in an inconsistent mid-update state, and any
      // subsequent WS messages would be queued behind the broken
      // listener invocation.
      try {
        messageHandler(event);
      } catch (err) {
        const snippet: string = typeof event.data === "string"
          ? event.data.slice(0, 200)
          : String(event.data).slice(0, 200);
        console.error("[openproxy] live-logs WS message handler threw:", err, "message snippet:", snippet);
      }
    }
    // F2: fan the parsed envelope out to ws-bus subscribers. The
    // logs handler above has already updated `state.logs` (e.g.
    // `lastSeenId` from a `row` envelope), so subscribers see a
    // consistent snapshot. The bus is independent of the logs view
    // — subscribers for `notification` (F4 tray), `row` (F5 live-
    // store), etc. register via `subscribeWs` in `state/ws-bus.ts`.
    //
    // We re-parse the JSON here (the logs handler parses its own
    // copy). The duplicate parse is a few KB per message — negligible
    // — and decoupling the two paths is worth it (the logs handler
    // can stay focused on logs without having to forward the parsed
    // object back to the bus). Malformed JSON is silently skipped:
    // the logs handler already showed a toast for it.
    if (typeof event.data === "string") {
      try {
        const parsed: unknown = JSON.parse(event.data);
        if (
          parsed !== null &&
          typeof parsed === "object" &&
          "type" in parsed
        ) {
          const envelope = parsed as { type: unknown };
          if (typeof envelope.type === "string") {
            // dispatchWs is wrapped in try/catch internally per
            // subscriber, so it never throws.
            dispatchWs(parsed as WsEnvelope);
          }
        }
      } catch (_e) {
        // Malformed JSON — logs handler already toasted. Skip dispatch.
      }
    }
  });
  ws.addEventListener("close", () => {
    if (state.logs.ws !== ws) return;
    clearInterval(heartbeatHandle);
    setLogsStatus("disconnected");
    scheduleLogsReconnect();
  });
  ws.addEventListener("error", () => {
    if (state.logs.ws !== ws) return;
    ws.close();
  });
  state.logs.ws = ws;
}

export function disconnectLogsWebSocket(): void {
  clearLogsReconnectTimer();
  if (state.logs.ws) {
    try { state.logs.ws.close(); } catch (_e: unknown) { /* already closed */ }
    state.logs.ws = null;
  }
  setLogsStatus("disconnected");
}
