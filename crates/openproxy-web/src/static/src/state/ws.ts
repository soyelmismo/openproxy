// state/ws.ts — WebSocket lifecycle for the live-logs view. The
// singleton guard and the reconnect backoff live here; the
// message routing is in views/logs.js.

import { state } from "./index.js";
import { LOGS_WS_RECONNECT_DELAYS } from "../lib/constants.js";
import type { StageEvent } from "../lib/types/api.js";

/** Connection status for the live-logs view. */
export type LogsStatus = "connected" | "connecting" | "reconnecting" | "disconnected";

export function logsWsUrl(): string {
  const scheme: "ws:" | "wss:" = location.protocol === "https:" ? "wss:" : "ws:";
  return `${scheme}//${location.host}/web/api/usage/stream`;
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
  ws.addEventListener("open", () => {
    if (state.logs.ws !== ws) return;
    state.logs.reconnectAttempt = 0;
    setLogsStatus("connected");
    if (state.logs.lastSeenId > 0) {
      ws.send(JSON.stringify({ type: "subscribe", since_id: state.logs.lastSeenId }));
    }
  });
  ws.addEventListener("message", (event: MessageEvent) => {
    if (state.logs.ws !== ws) return;
    if (typeof messageHandler !== "function") return;
    // CRITICAL: wrap the entire handler in try/catch. Without this,
    // a single malformed WS message (e.g. an unexpected null field
    // in a usage row) would throw out of `messageHandler`, leave
    // `state.logs` in an inconsistent mid-update state, and any
    // subsequent WS messages would be queued behind the broken
    // listener invocation — manifesting as "second request doesn't
    // appear in real-time after a failure". The DOM spec doesn't
    // close the WS on an uncaught exception in an event listener,
    // but it DOES skip any remaining work in that listener
    // invocation, and the cascade of inconsistent state can break
    // subsequent renders. Catching here keeps the listener alive
    // and logs the failure for debugging.
    try {
      messageHandler(event);
    } catch (err) {
      // Log to console with a snippet of the message data for
      // debugging, but don't rethrow — the next WS message must
      // be allowed to process normally.
      const snippet: string = typeof event.data === "string"
        ? event.data.slice(0, 200)
        : String(event.data).slice(0, 200);
      console.error("[openproxy] live-logs WS message handler threw:", err, "message snippet:", snippet);
    }
  });
  ws.addEventListener("close", () => {
    if (state.logs.ws !== ws) return;
    setLogsStatus("disconnected");
    scheduleLogsReconnect();
  });
  ws.addEventListener("error", () => {
    if (state.logs.ws !== ws) return;
    // `__logMsgTrace` is an opt-in debug flag set by the host page
    // for verbose logging. Not part of the Window type, so we read
    // it through a narrow local cast.
    const traceFlag: unknown = (window as unknown as Record<string, unknown>)["__logMsgTrace"];
    if (typeof traceFlag !== "undefined") {
      // The error is fine — the close event will follow.
    }
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
