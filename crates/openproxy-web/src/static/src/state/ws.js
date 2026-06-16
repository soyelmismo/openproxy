// state/ws.js — WebSocket lifecycle for the live-logs view. The
// singleton guard and the reconnect backoff live here; the
// message routing is in views/logs.js.

import { state } from "./index.js";
import { LOGS_WS_RECONNECT_DELAYS } from "../lib/constants.js";

export function logsWsUrl() {
  const scheme = location.protocol === "https:" ? "wss:" : "ws:";
  return `${scheme}//${location.host}/web/api/usage/stream`;
}

export function setLogsStatus(status) {
  state.logs.status = status;
  const badge = document.getElementById("logs-connection-status");
  if (!badge) return;
  const labels = {
    connected: "🟢 connected",
    connecting: "🟡 connecting",
    reconnecting: "🟡 reconnecting",
    disconnected: "🔴 disconnected",
  };
  badge.className = `logs-connection-badge ${status}`;
  badge.textContent = labels[status] || "🔴 disconnected";
}

function clearLogsReconnectTimer() {
  if (state.logs.reconnectTimer) {
    clearTimeout(state.logs.reconnectTimer);
    state.logs.reconnectTimer = null;
  }
}

function scheduleLogsReconnect() {
  clearLogsReconnectTimer();
  const delay = LOGS_WS_RECONNECT_DELAYS[Math.min(state.logs.reconnectAttempt, LOGS_WS_RECONNECT_DELAYS.length - 1)];
  state.logs.reconnectAttempt += 1;
  state.logs.reconnectTimer = setTimeout(connectLogsWebSocket, delay);
}

// Connected message handler. Set by views/logs.js during mount.
let messageHandler = null;
export function setMessageHandler(fn) { messageHandler = fn; }

export function connectLogsWebSocket() {
  clearLogsReconnectTimer();
  if (state.logs.ws) {
    const ready = state.logs.ws.readyState;
    if (ready === WebSocket.OPEN) { setLogsStatus("connected"); return; }
    if (ready === WebSocket.CONNECTING) return;
  }
  setLogsStatus(state.logs.reconnectAttempt === 0 ? "connecting" : "reconnecting");
  const ws = new WebSocket(logsWsUrl());
  ws.addEventListener("open", () => {
    if (state.logs.ws !== ws) return;
    state.logs.reconnectAttempt = 0;
    setLogsStatus("connected");
    if (state.logs.lastSeenId > 0) {
      ws.send(JSON.stringify({ type: "subscribe", since_id: state.logs.lastSeenId }));
    }
  });
  ws.addEventListener("message", (event) => {
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
    if (typeof window !== "undefined" && window.__logMsgTrace) {
      // The error is fine — the close event will follow.
    }
    ws.close();
  });
  state.logs.ws = ws;
}

export function disconnectLogsWebSocket() {
  clearLogsReconnectTimer();
  if (state.logs.ws) {
    try { state.logs.ws.close(); } catch (_) { /* already closed */ }
    state.logs.ws = null;
  }
  setLogsStatus("disconnected");
}
