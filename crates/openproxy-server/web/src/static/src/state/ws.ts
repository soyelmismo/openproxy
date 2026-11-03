// Live-logs WebSocket lifecycle: singleton guard, reconnect backoff. Message
// routing lives in views/logs.js.

import { state } from "./index.js";
import { LOGS_WS_RECONNECT_DELAYS } from "../lib/constants.js";
import type { StageEvent } from "../lib/types/api.js";
import { dispatchWs } from "./ws-bus.js";
import { liveLogsStore } from "./live-logs-store.js";
import type { WsEnvelope } from "../views/logs.js";
import { getToken } from "./auth.js";
import { api } from "../lib/api.js";

/** Connection status for the live-logs view. */
export type LogsStatus = "connected" | "connecting" | "reconnecting" | "disconnected";

const statusSubscribers = new Set<(status: LogsStatus) => void>();

export function subscribeLogsStatus(fn: (status: LogsStatus) => void): () => void {
  statusSubscribers.add(fn);
  fn(state.logs.status as LogsStatus);
  return () => statusSubscribers.delete(fn);
}

/** Base URL of the live-logs WebSocket (no credentials). */
export function logsWsUrl(): string {
  const scheme: "ws:" | "wss:" = location.protocol === "https:" ? "wss:" : "ws:";
  // Served by the openproxy server itself at `/admin/ws`.
  return `${scheme}//${location.host}/admin/ws`;
}

/** WebSocket URL carrying a single-use handshake ticket.
 *
 *  The manage-scope API key never goes in the URL: `new WebSocket()` cannot
 *  set headers, and query strings land verbatim in reverse-proxy access logs.
 *  The server mints a 30-second ticket bound to the key
 *  (`POST /admin/api/ws-ticket`, normal Bearer header) and the upgrade
 *  redeems it via `?ticket=`. */
export function logsWsUrlWithTicket(ticket: string): string {
  return `${logsWsUrl()}?ticket=${encodeURIComponent(ticket)}`;
}

interface WsTicketResponse { ticket?: unknown }

/** Fetch a handshake ticket. Null on any failure so the caller schedules a
 *  reconnect instead of throwing. */
async function fetchWsTicket(): Promise<string | null> {
  try {
    const res = await api("/ws-ticket", { method: "POST" }) as WsTicketResponse | null;
    const ticket: unknown = res?.ticket;
    return typeof ticket === "string" && ticket.length > 0 ? ticket : null;
  } catch (err: unknown) {
    console.warn("[openproxy] live-logs WS ticket request failed:", err);
    return null;
  }
}

export function setLogsStatus(status: LogsStatus): void {
  state.logs.status = status;
  for (const subscriber of statusSubscribers) subscriber(status);
  const badge: HTMLElement | null = document.getElementById("logs-connection-status");
  if (!badge) return;
  const labels = {
    connected: "🟢 connected",
    connecting: "🟡 connecting",
    reconnecting: "🟡 reconnecting",
    disconnected: "🔴 disconnected",
  } satisfies Record<LogsStatus, string>;
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

/** Type guard for StageEvent. Anything off-shape is ignored. */
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

// Guards against a second connect opening a socket before the first exists.
let ticketInFlight = false;
// A ticket resolving after a disconnect or newer attempt is discarded.
let connectGeneration = 0;

export function connectLogsWebSocket(): void {
  clearLogsReconnectTimer();
  if (!getToken()) {
    setLogsStatus("disconnected");
    return;
  }
  if (state.logs.ws) {
    const ready: number = state.logs.ws.readyState;
    if (ready === WebSocket.OPEN) { setLogsStatus("connected"); return; }
    if (ready === WebSocket.CONNECTING) return;
  }
  if (ticketInFlight) return;
  setLogsStatus(state.logs.reconnectAttempt === 0 ? "connecting" : "reconnecting");
  ticketInFlight = true;
  const generation: number = ++connectGeneration;
  void fetchWsTicket().then((ticket: string | null) => {
    ticketInFlight = false;
    if (generation !== connectGeneration) return; // superseded / disconnected
    if (!ticket) {
      setLogsStatus("disconnected");
      scheduleLogsReconnect();
      return;
    }
    openLogsWebSocket(logsWsUrlWithTicket(ticket));
  });
}

function openLogsWebSocket(url: string): void {
  const ws: WebSocket = new WebSocket(url);
  // Ping every 15s; force-close after 30s without traffic. Detects half-open
  // TCP (network change, laptop sleep, a proxy dropping the WS), which would
  // otherwise leave the dashboard "connected" and silent.
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
    // No traffic for 30s: the close handler drives the reconnect.
    if (Date.now() - lastPong > 30_000) {
      console.warn("[openproxy] live-logs WS heartbeat timeout — no pong in 30s, forcing reconnect");
      try { ws.close(); } catch { /* already closed */ }
      clearInterval(heartbeatHandle);
      return;
    }
    try {
      ws.send(JSON.stringify({ type: "ping" }));
    } catch {
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
    if (liveLogsStore.lastAppliedCursor > 0) {
      ws.send(JSON.stringify({ type: "subscribe", cursor: liveLogsStore.lastAppliedCursor }));
    }
  });
  ws.addEventListener("message", (event: MessageEvent) => {
    if (state.logs.ws !== ws) return;
    // Any inbound message counts as liveness, not just a pong.
    lastPong = Date.now();
    if (typeof messageHandler === "function") {
      // A throw out of the handler would leave `state.logs` mid-update and
      // queue every later message behind the broken listener.
      try {
        messageHandler(event);
      } catch (err) {
        const snippet: string = typeof event.data === "string"
          ? event.data.slice(0, 200)
          : String(event.data).slice(0, 200);
        console.error("[openproxy] live-logs WS message handler threw:", err, "message snippet:", snippet);
      }
    }
    // Fan the parsed envelope out to ws-bus subscribers. They see a
    // consistent snapshot: the logs handler has already applied its own
    // update (e.g. `lastSeenId` from a `row` envelope).
    //
    // The re-parse duplicates the logs handler's work, a few KB per message,
    // which buys a bus path independent of the logs view.
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
            // dispatchWs catches per subscriber, so it never throws.
            dispatchWs(parsed as WsEnvelope);
          }
        }
      } catch {
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
  connectGeneration += 1; // invalidate any in-flight ticket request
  if (state.logs.ws) {
    try { state.logs.ws.close(); } catch { /* already closed */ }
    state.logs.ws = null;
  }
  setLogsStatus("disconnected");
}
