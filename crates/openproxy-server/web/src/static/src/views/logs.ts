import { html, type TemplateResult } from "lit-html";
import { repeat } from "lit-html/directives/repeat.js";
import { live } from "lit-html/directives/live.js";
import { state } from "../state/index.js";
import { renderLogRowHtml } from "../components/log-row.js";
import { icons } from "../lib/icons.js";
import { LOG_COLUMNS, LOGS_VISIBLE_COLUMNS_STORAGE_KEY } from "../lib/constants.js";
import {
  connectLogsWebSocket,
  setMessageHandler,
  disconnectLogsWebSocket,
} from "../state/ws.js";
import { fetchRecordingState, toggleRecording } from "../handlers/log-handlers.js";
import { mountView, requestUpdate } from "../state/reactive.js";
import { openLogDetail } from "../components/log-detail.js";
import { liveLogsStore, type AttemptState } from "../state/live-logs-store.js";
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

import { exportLogsCSV } from "../handlers/log-handlers.js";

let filterSearch: string = "";
let filterStatus: string = "all";
let isPaused: boolean = false;
let searchDebounceTimer: ReturnType<typeof setTimeout> | null = null;
let frozenLogsSnapshot: AttemptState[] | null = null;

function onSearchInput(e: Event): void {
  const target = e.target as HTMLInputElement;
  const val = target.value.trim().toLowerCase();
  if (searchDebounceTimer) clearTimeout(searchDebounceTimer);
  searchDebounceTimer = setTimeout(() => {
    filterSearch = val;
    frozenLogsSnapshot = null;
    state.logs.page = 1;
    state.logs.followTail = true;
    requestUpdate();
  }, 120);
}

function onClearSearch(): void {
  if (searchDebounceTimer) clearTimeout(searchDebounceTimer);
  filterSearch = "";
  frozenLogsSnapshot = null;
  state.logs.page = 1;
  state.logs.followTail = true;
  requestUpdate();
}

function onSetStatusFilter(status: string): void {
  filterStatus = status;
  frozenLogsSnapshot = null;
  state.logs.page = 1;
  state.logs.followTail = true;
  requestUpdate();
}

function onTogglePause(): void {
  isPaused = !isPaused;
  requestUpdate();
}

function handleLogsMessage(event: MessageEvent): void {
  if (isPaused) return;
  try {
    const env = JSON.parse(event.data) as WsEnvelope;
    // Pass directly to the store and request reactive render update
    liveLogsStore.dispatch(env);
    requestUpdate();
  } catch (e) {
    // Ignore invalid JSON
  }
}

function matchesLogFilter(r: { upstreamModelId?: string; providerId?: string; requestId?: string; traceId?: string; error?: string | null; statusCode?: number | null; terminalKind?: string | null }): boolean {
  if (filterSearch) {
    const model = (r.upstreamModelId || "").toLowerCase();
    const prov = (r.providerId || "").toLowerCase();
    const req = (r.requestId || "").toLowerCase();
    const trace = (r.traceId || "").toLowerCase();
    const err = (r.error || "").toLowerCase();
    if (!model.includes(filterSearch) && !prov.includes(filterSearch) && !req.includes(filterSearch) && !trace.includes(filterSearch) && !err.includes(filterSearch)) {
      return false;
    }
  }
  if (filterStatus === "inflight") {
    return r.statusCode == null && !r.terminalKind;
  } else if (filterStatus === "2xx") {
    return r.statusCode != null && r.statusCode >= 200 && r.statusCode < 300;
  } else if (filterStatus === "4xx") {
    return r.statusCode != null && r.statusCode >= 400 && r.statusCode < 500;
  } else if (filterStatus === "5xx") {
    return r.statusCode != null && r.statusCode >= 500;
  } else if (filterStatus === "error") {
    return (r.statusCode != null && r.statusCode >= 400) || Boolean(r.error) || r.terminalKind === "failed";
  }
  return true;
}

function renderHeaderRow(visibleColKeys: Set<string>): TemplateResult {
  return html`<div class="log-row desktop-table-header" style="cursor:default;border-bottom:1px solid var(--color-border);font-weight:600;font-size:0.72rem;text-transform:uppercase;color:var(--color-log-header-fg);background:var(--color-log-header-bg);position:sticky;top:0;z-index:1;">${LOG_COLUMNS
    .filter((c) => visibleColKeys.has(c.key))
    .map((c) => html`<span class="log-${c.key}" data-col=${c.key}>${c.label}</span>`)}</div>`;
}

function renderPagination(totalRows: number, totalP: number): TemplateResult {
  if (totalRows === 0) return html``;
  const isFirst = state.logs.page <= 1;
  const isLast = state.logs.page >= totalP;
  return html`<div class="logs-pagination">
    <div class="mobile-pag-row-controls">
      <button ?disabled=${isFirst} @click=${() => logsGoPage(1)} title="First page">${icons.chevronsLeft()}</button>
      <button ?disabled=${isFirst} @click=${logsPrevPage} title="Previous page">${icons.chevronLeft()} Prev</button>
      <span class="page-info">Page ${state.logs.page} of ${totalP}</span>
      <button ?disabled=${isLast} @click=${logsNextPage} title="Next page">Next ${icons.chevronRight()}</button>
      <button ?disabled=${isLast} @click=${() => logsGoPage(totalP)} title="Last page">${icons.chevronsRight()}</button>
    </div>
    <div class="mobile-pag-row-meta">
      <span class="rows-info">${totalRows} row${totalRows !== 1 ? "s" : ""}</span>
      <label class="logs-follow-toggle" title="When ON, new rows automatically scroll the view to the most recent page.">
        <input type="checkbox" id="logs-follow-input" ?checked=${state.logs.followTail} @change=${logsSetFollow}>
        <span>Follow</span>
      </label>
    </div>
  </div>`;
}

function renderColumnsMenu(): TemplateResult {
  const visible = state.logs.visibleColumns || new Set(LOG_COLUMNS.map((c) => c.key));
  return html`<div class="columns-menu ${columnsMenuOpen ? "open" : ""}" role="menu">${LOG_COLUMNS.map((c) => html`<label class="columns-menu-item"><input type="checkbox" data-arg1="${c.key}" .checked=${live(visible.has(c.key))} @change=${(e: Event) => onColumnToggle(c.key, e)}><span>${c.label}</span></label>`)}</div>`;
}

function renderLogsView(): TemplateResult {
  const allInflight = liveLogsStore.selectInflightRows();
  
  // Si estamos en página > 1 y tenemos snapshot congelado, usamos el snapshot inmóvil
  const sourceFinished = (state.logs.page > 1 && frozenLogsSnapshot)
    ? frozenLogsSnapshot
    : liveLogsStore.selectFinishedRows();

  const inflightRows = allInflight.filter(matchesLogFilter);
  const finishedRows = sourceFinished.filter(matchesLogFilter);

  const totalFinished = finishedRows.length;
  const rpp = state.logs.rowsPerPage;
  const totalP = Math.max(1, Math.ceil(totalFinished / rpp));
  
  if (state.logs.followTail) {
    state.logs.page = 1;
  } else if (state.logs.page > totalP) {
    state.logs.page = totalP;
  }
  
  if (state.logs.page < 1) state.logs.page = 1;
  
  const start = (state.logs.page - 1) * rpp;
  const end = Math.min(start + rpp, totalFinished);
  const pageFinishedRows = finishedRows.slice(start, end);
  
  const visibleColKeys = state.logs.visibleColumns || new Set(LOG_COLUMNS.map((c) => c.key));
  const nowOffsetMs = clockStore.nowMs - liveLogsStore.clockOffsetMs;

  return html`
    <div class="logs-header">
      <div class="m-header-top-row">
        <h2>Live Logs</h2>
        <div class="m-header-status-badges">
          <span id="logs-connection-status" class="logs-connection-badge disconnected"><span class="status-dot"></span> disconnected</span>
          <button id="logs-recording-toggle" class="logs-recording-toggle" type="button" @click=${onRecordingToggleClick}>
            <span class="logs-recording-dot" aria-hidden="true">${icons.record()}</span>
            <span class="logs-recording-label">Record: <strong>OFF</strong></span>
          </button>
        </div>
      </div>
      <div class="logs-header-actions m-header-actions-row">
        <button type="button" class="btn btn-sm ${isPaused ? "btn-warn" : ""}" @click=${onTogglePause} title=${isPaused ? "Resume live streaming" : "Pause live stream to inspect"}>
          ${isPaused ? html`${icons.play()} Resume` : html`${icons.pause()} Pause`}
        </button>
        <button type="button" class="btn btn-sm" @click=${exportLogsCSV} title="Export logs as CSV file">
          ${icons.export()} Export CSV
        </button>
        <div class="columns-menu-wrapper">
          <button id="logs-columns-toggle" type="button" class="logs-columns-toggle" aria-haspopup="true" aria-expanded=${columnsMenuOpen ? "true" : "false"} @click=${onToggleColumnsMenu}>
            <span>Columns</span>
            <span class="logs-columns-caret" aria-hidden="true">${icons.caretDown()}</span>
          </button>
          ${renderColumnsMenu()}
        </div>
      </div>
    </div>

    <!-- Live Logs Filter Toolbar -->
    <div class="logs-filter-toolbar">
      <div class="logs-search-box logs-search-wrapper">
        <span class="logs-search-icon" aria-hidden="true">${icons.search()}</span>
        <input
          type="search"
          class="logs-search-input"
          placeholder="Filter by model, provider, request id, trace..."
          .value=${filterSearch}
          @input=${onSearchInput}
        />
        ${filterSearch ? html`<button type="button" class="logs-search-clear" @click=${onClearSearch} aria-label="Clear filter">${icons.close()}</button>` : null}
      </div>
      <div class="logs-status-filters filter-bar" role="group" aria-label="Status filter">
        <button type="button" class="logs-filter-btn ${filterStatus === "all" ? "active" : ""}" @click=${() => onSetStatusFilter("all")}>All</button>
        <button type="button" class="logs-filter-btn ${filterStatus === "inflight" ? "active" : ""}" @click=${() => onSetStatusFilter("inflight")}>${icons.lightning()} In-flight</button>
        <button type="button" class="logs-filter-btn ${filterStatus === "2xx" ? "active" : ""}" @click=${() => onSetStatusFilter("2xx")}>2xx OK</button>
        <button type="button" class="logs-filter-btn ${filterStatus === "4xx" ? "active" : ""}" @click=${() => onSetStatusFilter("4xx")}>4xx Client</button>
        <button type="button" class="logs-filter-btn ${filterStatus === "5xx" ? "active" : ""}" @click=${() => onSetStatusFilter("5xx")}>5xx Server</button>
        <button type="button" class="logs-filter-btn ${filterStatus === "error" ? "active" : ""}" @click=${() => onSetStatusFilter("error")}>All Errors</button>
      </div>
    </div>

    <div class="logs" id="logs" @click=${onLogsClick}>
      <!-- Section 1: In Progress -->
      <div class="logs-section logs-section-inflight ${inflightRows.length === 0 ? "empty" : ""}" id="logs-section-inflight">
        <div class="logs-section-header">
          <span class="logs-section-title">
            <span class="logs-inflight-dot" aria-hidden="true"></span> Requests in progress
          </span>
          <span class="logs-section-count">${inflightRows.length}</span>
        </div>
        <div class="logs-scroll-area logs-scroll-area-inflight" id="logs-scroll-area-inflight">
          ${renderHeaderRow(visibleColKeys)}
          ${inflightRows.length === 0
            ? html`<div class="empty empty-inflight logs-empty-placeholder" style="padding:1.5rem;text-align:center;color:var(--color-text-muted);">${filterSearch || filterStatus !== "all" ? "No in-progress requests matching filter." : "No requests in progress."}</div>`
            : repeat(
                inflightRows,
                (r) => r.attemptKey,
                (r) => html`<div data-key=${r.attemptKey}>${renderLogRowHtml(r, visibleColKeys, nowOffsetMs)}</div>`
              )}
        </div>
      </div>

      <!-- Section 2: Finished or Erroneous -->
      <div class="logs-section logs-section-finished" id="logs-section-finished">
        <div class="logs-section-header">
          <span class="logs-section-title">
            Finished or failed
          </span>
          <span class="logs-section-count">${totalFinished}</span>
        </div>
        <div class="logs-scroll-area logs-scroll-area-finished" id="logs-scroll-area-finished">
          ${renderHeaderRow(visibleColKeys)}
          ${pageFinishedRows.length === 0
            ? html`<div class="empty empty-finished logs-empty-placeholder" style="padding:1.5rem;text-align:center;color:var(--color-text-muted);">${filterSearch || filterStatus !== "all" ? "No requests matching current filter." : "No recent requests yet."}</div>`
            : repeat(
                pageFinishedRows,
                (r) => r.attemptKey,
                (r) => html`<div data-key=${r.attemptKey}>${renderLogRowHtml(r, visibleColKeys, nowOffsetMs)}</div>`
              )}
        </div>
        ${renderPagination(totalFinished, totalP)}
      </div>
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
      clickedRow
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
    if (state.logs.page === 1) {
      frozenLogsSnapshot = null;
      state.logs.followTail = true;
    } else {
      state.logs.followTail = false;
    }
    requestUpdate();
  }
}
export function logsNextPage(): void {
  const finishedRows = liveLogsStore.selectFinishedRows();
  const totalP = Math.max(1, Math.ceil(finishedRows.length / state.logs.rowsPerPage));
  if (state.logs.page < totalP) {
    if (state.logs.page === 1) {
      frozenLogsSnapshot = [...finishedRows];
    }
    state.logs.page++;
    state.logs.followTail = false;
    requestUpdate();
  }
}
export function logsGoPage(p: number): void {
  const finishedRows = liveLogsStore.selectFinishedRows();
  const totalP = Math.max(1, Math.ceil(finishedRows.length / state.logs.rowsPerPage));
  const targetPage = Math.max(1, Math.min(p, totalP));

  if (targetPage === 1) {
    frozenLogsSnapshot = null;
    state.logs.followTail = true;
  } else {
    if (!frozenLogsSnapshot) {
      frozenLogsSnapshot = [...finishedRows];
    }
    state.logs.followTail = false;
  }

  state.logs.page = targetPage;
  requestUpdate();
}
export function logsSetFollow(e: Event): void {
  const target = e.target;
  let enabled = false;
  if (target instanceof HTMLInputElement) {
    enabled = !!target.checked;
  }
  state.logs.followTail = enabled;
  if (enabled) {
    frozenLogsSnapshot = null;
    state.logs.page = 1;
    requestUpdate();
  }
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
  frozenLogsSnapshot = null;

  const cleanupReactive = mountView(main, renderLogsView);

  const onDocClickForMenu = (ev: Event): void => {
    if (!columnsMenuOpen) return;
    const target = ev.target;
    if (!(target instanceof Element)) return;
    if (target.closest(".columns-menu-wrapper")) return;
    columnsMenuOpen = false;
    requestUpdate();
  };
  const w = window as Window & typeof globalThis & { __logsColumnsDocClickBound?: boolean };
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
