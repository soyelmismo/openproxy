import { html, type TemplateResult } from "lit-html";
import { repeat } from "lit-html/directives/repeat.js";
import { live } from "lit-html/directives/live.js";
import { state } from "../state/index.js";
import { renderLogRowHtml } from "../components/log-row.js";
import { LOG_COLUMNS, LOGS_VISIBLE_COLUMNS_STORAGE_KEY } from "../lib/constants.js";
import {
  connectLogsWebSocket,
  setMessageHandler,
  disconnectLogsWebSocket,
} from "../state/ws.js";
import { fetchRecordingState, toggleRecording } from "../components/recording-toggle.js";
import { mountView, requestUpdate } from "../state/reactive.js";
import { openLogDetail } from "../components/log-detail.js";
import { liveLogsStore } from "../state/live-logs-store.js";
import { clockStore } from "../state/clock-store.js";
import type { RecentUsageRow, StageEvent } from "../lib/types/api.js";
import type { NotificationEvent } from "../lib/types/notifications.js";

// Keep legacy WsEnvelope for compatibility with ws-bus.ts and notifications
export interface WsEnvelope {
  type: "history" | "row" | "stage" | "lag_warning" | "resync" | "pong" | "error" | "notification" | "snapshot" | "attempt_event" | "usage_row" | "gap";
  data?: StageEvent | RecentUsageRow | NotificationEvent;
  row?: unknown;
  rows?: RecentUsageRow[];
  message?: string;
  request_id?: string;
  delta?: string;
  complete?: boolean;
  id?: number;
  skipped?: number;
  channel?: "usage" | "stage" | "notifications";
  since_id?: number;
  server_time?: string;
}

let columnsMenuOpen: boolean = false;

function loadVisibleColumns(): Set<string> {
  const allKeys = LOG_COLUMNS.map((c) => c.key);
  let result = new Set(allKeys);
  try {
    const raw = localStorage.getItem(LOGS_VISIBLE_COLUMNS_STORAGE_KEY);
    if (raw) {
      const parsed: unknown = JSON.parse(raw);
      if (Array.isArray(parsed)) {
        const valid = parsed.filter((k): k is string => typeof k === "string" && allKeys.includes(k));
        if (valid.length > 0) result = new Set(valid);
      }
    }
  } catch (_e) {
    result = new Set(allKeys);
  }
  return result;
}

function saveVisibleColumns(): void {
  const cols = state.logs.visibleColumns;
  if (!cols) return;
  try {
    localStorage.setItem(LOGS_VISIBLE_COLUMNS_STORAGE_KEY, JSON.stringify(Array.from(cols)));
  } catch (_e) {}
}

function handleLogsMessage(event: MessageEvent): void {
  try {
    const env = JSON.parse(event.data) as WsEnvelope;
    // Pass directly to the store
    liveLogsStore.dispatch(env);
  } catch (e) {
    // Ignore invalid JSON
  }
}

function renderHeaderRow(visibleColKeys: Set<string>): TemplateResult {
  return html`<div class="log-row" style="cursor:default;border-bottom:1px solid var(--color-border);font-weight:600;font-size:0.72rem;text-transform:uppercase;color:var(--color-text-muted);background:var(--color-log-header-bg);position:sticky;top:0;z-index:1;">${LOG_COLUMNS
    .filter((c) => visibleColKeys.has(c.key))
    .map((c) => html`<span class="log-${c.key}" data-col=${c.key}>${c.label}</span>`)}</div>`;
}

function renderPagination(totalRows: number, totalP: number): TemplateResult {
  if (totalRows === 0) return html``;
  const isFirst = state.logs.page <= 1;
  const isLast = state.logs.page >= totalP;
  return html`<div class="logs-pagination">
    <span class="rows-info">${totalRows} row${totalRows !== 1 ? "s" : ""}</span>
    <button ?disabled=${isFirst} @click=${() => logsGoPage(1)}>⟨⟨</button>
    <button ?disabled=${isFirst} @click=${logsPrevPage}>‹ Prev</button>
    <span class="page-info">Page ${state.logs.page} of ${totalP}</span>
    <button ?disabled=${isLast} @click=${logsNextPage}>Next ›</button>
    <button ?disabled=${isLast} @click=${() => logsGoPage(totalP)}>⟩⟩</button>
    <label class="logs-follow-toggle" title="When ON, new rows automatically scroll the view to the most recent page.">
      <input type="checkbox" id="logs-follow-input" ?checked=${state.logs.followTail} @change=${logsSetFollow}>
      <span>Follow</span>
    </label>
  </div>`;
}

function renderColumnsMenu(): TemplateResult {
  const visible = state.logs.visibleColumns || new Set(LOG_COLUMNS.map((c) => c.key));
  return html`<div class="columns-menu ${columnsMenuOpen ? "open" : ""}" role="menu">${LOG_COLUMNS.map((c) => html`<label class="columns-menu-item"><input type="checkbox" data-arg1="${c.key}" .checked=${live(visible.has(c.key))} @change=${(e: Event) => onColumnToggle(c.key, e)}><span>${c.label}</span></label>`)}</div>`;
}

function renderLogsView(): TemplateResult {
  const allRows = liveLogsStore.selectLogRows();
  const totalRows = allRows.length;
  const rpp = state.logs.rowsPerPage;
  const totalP = Math.max(1, Math.ceil(totalRows / rpp));
  
  if (state.logs.followTail) {
    state.logs.page = 1;
  } else if (state.logs.page > totalP) {
    state.logs.page = totalP;
  }
  
  if (state.logs.page < 1) state.logs.page = 1;
  
  const start = (state.logs.page - 1) * rpp;
  const end = Math.min(start + rpp, totalRows);
  const pageRows = allRows.slice(start, end);
  
  const visibleColKeys = state.logs.visibleColumns || new Set(LOG_COLUMNS.map((c) => c.key));

  return html`
    <div class="logs-header">
      <h2>Live Logs</h2>
      <div class="logs-header-actions">
        <div class="columns-menu-wrapper">
          <button id="logs-columns-toggle" type="button" class="logs-columns-toggle" aria-haspopup="true" aria-expanded=${columnsMenuOpen ? "true" : "false"} @click=${onToggleColumnsMenu}>
            <span>Columns</span>
            <span class="logs-columns-caret" aria-hidden="true">▾</span>
          </button>
          ${renderColumnsMenu()}
        </div>
        <span id="logs-connection-status" class="logs-connection-badge disconnected">🔴 disconnected</span>
        <button id="logs-recording-toggle" class="logs-recording-toggle" type="button" @click=${onRecordingToggleClick}>
          <span class="logs-recording-dot" aria-hidden="true"></span>
          <span class="logs-recording-label">⏺ Record: <strong>OFF</strong></span>
        </button>
      </div>
    </div>
    <div class="logs" id="logs" @click=${onLogsClick}>
      <div class="logs-scroll-area" id="logs-scroll-area">
        ${renderHeaderRow(visibleColKeys)}
        ${pageRows.length === 0
          ? html`<div class="empty" style="padding:2rem;">No recent requests yet.</div>`
          : repeat(
              pageRows,
              (r) => r.attemptKey,
              (r) => html`<div data-key=${r.attemptKey}>${renderLogRowHtml(r, visibleColKeys, clockStore.nowMs - liveLogsStore.clockOffsetMs)}</div>`
            )}
      </div>
      ${renderPagination(totalRows, totalP)}
    </div>
  `;
}

function onLogsClick(e: Event): void {
  const target = e.target;
  if (!(target instanceof Element)) return;
  const rowEl = target.closest(".log-row[data-request-id]");
  if (!rowEl) return;
  const el = rowEl as HTMLElement;
  const id = el.dataset["id"];
  const attemptKey = el.dataset["attemptKey"];
  
  const identity = id ? { kind: "row_id" as const, id: Number(id) } : (attemptKey ? { kind: "attempt" as const, attemptKey } : null);
  if (!identity) return;
  
  const clickedRow = liveLogsStore.selectDetail(identity);
  if (clickedRow) {
    void openLogDetail(
      clickedRow.rowId ? String(clickedRow.rowId) : "",
      clickedRow.requestId,
      clickedRow.traceId,
      clickedRow as any
    );
  }
}

function onToggleColumnsMenu(): void {
  columnsMenuOpen = !columnsMenuOpen;
  requestUpdate();
}

function onColumnToggle(key: string, e: Event): void {
  e.stopPropagation();
  toggleColumn(key);
}

function onRecordingToggleClick(): void {
  void toggleRecording();
}

export function logsPrevPage(): void {
  if (state.logs.page > 1) {
    state.logs.page--;
    if (state.logs.page === 1) state.logs.followTail = true;
    requestUpdate();
  }
}
export function logsNextPage(): void {
  const allRows = liveLogsStore.selectLogRows();
  const totalP = Math.max(1, Math.ceil(allRows.length / state.logs.rowsPerPage));
  if (state.logs.page < totalP) {
    state.logs.page++;
    if (state.logs.page >= totalP) state.logs.followTail = false;
    requestUpdate();
  }
}
export function logsGoPage(p: number): void {
  const allRows = liveLogsStore.selectLogRows();
  const totalP = Math.max(1, Math.ceil(allRows.length / state.logs.rowsPerPage));
  state.logs.page = Math.max(1, Math.min(p, totalP));
  state.logs.followTail = (state.logs.page === 1);
  requestUpdate();
}
export function logsSetFollow(e: Event): void {
  const target = e.target;
  let enabled = false;
  if (target instanceof HTMLInputElement) {
    enabled = !!target.checked;
  }
  state.logs.followTail = enabled;
  if (enabled) { state.logs.page = 1; requestUpdate(); }
}

export function toggleColumnsMenu(): void {
  columnsMenuOpen = !columnsMenuOpen;
  requestUpdate();
}

export function toggleColumn(key: string): void {
  if (!state.logs.visibleColumns) {
    state.logs.visibleColumns = new Set(LOG_COLUMNS.map((c) => c.key));
  }
  const set = state.logs.visibleColumns;
  if (set.has(key)) {
    if (set.size === 1) {
      requestUpdate();
      return;
    }
    set.delete(key);
  } else {
    set.add(key);
  }
  saveVisibleColumns();
  requestUpdate();
}

export async function mountLogs(): Promise<(() => void) | void> {
  const main = document.getElementById("main");
  if (!main) return;

  columnsMenuOpen = false;

  if (!state.logs.visibleColumns) {
    state.logs.visibleColumns = loadVisibleColumns();
  }
  
  state.logs.page = 1;
  state.logs.followTail = true;

  const cleanupReactive = mountView(main, renderLogsView);

  const onDocClickForMenu = (ev: Event): void => {
    if (!columnsMenuOpen) return;
    const target = ev.target;
    if (!(target instanceof Element)) return;
    if (target.closest(".columns-menu-wrapper")) return;
    columnsMenuOpen = false;
    requestUpdate();
  };
  const w = window as unknown as { __logsColumnsDocClickBound?: boolean };
  if (!w.__logsColumnsDocClickBound) {
    document.addEventListener("click", onDocClickForMenu);
    w.__logsColumnsDocClickBound = true;
  }

  fetchRecordingState();
  setMessageHandler(handleLogsMessage);
  connectLogsWebSocket();
  
  clockStore.subscribe(requestUpdate);

  const hash = location.hash || "";
  const qIdx = hash.indexOf("?");
  if (qIdx >= 0) {
    const params = new URLSearchParams(hash.slice(qIdx + 1));
    const traceId = params.get("trace_id") || "";
    const requestId = params.get("request_id") || "";
    if (traceId || requestId) {
      void openLogDetail("", requestId, traceId);
    }
  }

  return () => {
    disconnectLogsWebSocket();
    clockStore.unsubscribe(requestUpdate);
    cleanupReactive();
  };
}

setMessageHandler(handleLogsMessage);
