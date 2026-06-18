// state/ws.ts — WebSocket lifecycle for the live-logs view. The
// singleton guard and the reconnect backoff live here; the
// message routing is in views/logs.js.

import { state } from "./index.js";
import { LOGS_WS_RECONNECT_DELAYS } from "../lib/constants.js";

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
    if (typeof messageHandler === "function") messageHandler(event);
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
