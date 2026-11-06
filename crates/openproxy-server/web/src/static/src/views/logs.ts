import { html, type TemplateResult } from "lit-html";
import { repeat } from "lit-html/directives/repeat.js";
import { live } from "lit-html/directives/live.js";
import { state } from "../state/index.js";
import { renderLogRowHtml } from "../components/log-row.js";
import { icons } from "../lib/icons.js";
import { LOG_COLUMNS, LOGS_VISIBLE_COLUMNS_STORAGE_KEY } from "../lib/constants.js";
import { connectLogsWebSocket, setMessageHandler, disconnectLogsWebSocket } from "../state/ws.js";
import { fetchRecordingState, toggleRecording, exportLogsCSV } from "../handlers/log-handlers.js";
import { api } from "../lib/api.js";
import { mountView, requestUpdate } from "../state/reactive.js";
import { openLogDetail } from "../components/log-detail/index.js";
import { liveLogsStore, type AttemptState } from "../state/live-logs-store.js";
import { clockStore } from "../state/clock-store.js";
import type { RecentUsageRow, StageEvent } from "../lib/types/api.js";
import type { NotificationEvent } from "../lib/types/notifications.js";

export interface ModelsRefreshedEventData {
  provider_id: string; models_refreshed: number; new_model_ids?: string[]; models_activated?: number;
}

export interface WsEnvelope {
  type: "history" | "row" | "stage" | "lag_warning" | "resync" | "pong" | "error" | "notification" | "snapshot" | "attempt_event" | "usage_row" | "gap" | "models_refreshed";
  data?: StageEvent | RecentUsageRow | NotificationEvent | ModelsRefreshedEventData;
  row?: unknown; rows?: RecentUsageRow[]; message?: string; request_id?: string; delta?: string; complete?: boolean; id?: number; skipped?: number; channel?: "usage" | "stage" | "notifications"; since_id?: number; server_time?: string;
}

let columnsMenuOpen = false;

function loadVisibleColumns(): Set<string> {
  const allKeys = LOG_COLUMNS.map((c) => c.key);
  try {
    const raw = localStorage.getItem(LOGS_VISIBLE_COLUMNS_STORAGE_KEY);
    if (raw) {
      const parsed: unknown = JSON.parse(raw);
      if (Array.isArray(parsed)) {
        const valid = parsed.filter((k): k is string => typeof k === "string" && allKeys.includes(k));
        if (valid.length > 0) return new Set(valid);
      }
    }
  } catch { /* fallback */ }
  return new Set(allKeys);
}

function saveVisibleColumns(): void {
  if (state.logs.visibleColumns) {
    try { localStorage.setItem(LOGS_VISIBLE_COLUMNS_STORAGE_KEY, JSON.stringify(Array.from(state.logs.visibleColumns))); } catch { /* ignore */ }
  }
}

let filterSearch = "";
let filterStatus = "all";
let filterApiKeyId = "all";
let isPaused = false;
let searchDebounceTimer: ReturnType<typeof setTimeout> | null = null;
let frozenLogsSnapshot: AttemptState[] | null = null;

function onSearchInput(e: Event): void {
  const val = (e.target as HTMLInputElement).value.trim().toLowerCase();
  if (searchDebounceTimer) clearTimeout(searchDebounceTimer);
  searchDebounceTimer = setTimeout(() => {
    filterSearch = val; frozenLogsSnapshot = null; state.logs.page = 1; state.logs.followTail = true; requestUpdate();
  }, 120);
}

function onSetStatusFilter(status: string): void {
  filterStatus = status; frozenLogsSnapshot = null; state.logs.page = 1; state.logs.followTail = true; requestUpdate();
}

function onSetApiKeyFilter(keyId: string): void {
  filterApiKeyId = keyId; frozenLogsSnapshot = null; state.logs.page = 1; state.logs.followTail = true; requestUpdate();
}

function handleLogsMessage(event: MessageEvent): void {
  if (isPaused) return;
  try { liveLogsStore.dispatch(JSON.parse(event.data) as WsEnvelope); requestUpdate(); } catch { /* ignore */ }
}

function matchesLogFilter(r: { upstreamModelId?: string; providerId?: string; requestId?: string; traceId?: string; error?: string | null; statusCode?: number | null; terminalKind?: string | null; apiKeyId?: number | null }): boolean {
  if (filterApiKeyId !== "all") {
    if (filterApiKeyId === "none") {
      if (r.apiKeyId != null) return false;
    } else {
      const targetId = Number(filterApiKeyId);
      if (r.apiKeyId !== targetId) return false;
    }
  }
  if (filterSearch) {
    const s = filterSearch;
    const hit = (r.upstreamModelId || "").toLowerCase().includes(s)
      || (r.providerId || "").toLowerCase().includes(s)
      || (r.requestId || "").toLowerCase().includes(s)
      || (r.traceId || "").toLowerCase().includes(s)
      || (r.error || "").toLowerCase().includes(s)
      || (r.apiKeyId != null && (`#${r.apiKeyId}`.includes(s) || String(r.apiKeyId) === s));
    if (!hit) return false;
  }
  if (filterStatus === "inflight") return r.statusCode == null && !r.terminalKind;
  if (filterStatus === "2xx") return r.statusCode != null && r.statusCode >= 200 && r.statusCode < 300;
  if (filterStatus === "4xx") return r.statusCode != null && r.statusCode >= 400 && r.statusCode < 500;
  if (filterStatus === "5xx") return r.statusCode != null && r.statusCode >= 500;
  if (filterStatus === "error") return (r.statusCode != null && r.statusCode >= 400) || Boolean(r.error) || r.terminalKind === "failed";
  return true;
}

function renderHeaderRow(visibleColKeys: Set<string>): TemplateResult {
  return html`<div class="log-row desktop-table-header" style="cursor:default;border-bottom:1px solid var(--color-border);font-weight:600;font-size:0.72rem;text-transform:uppercase;color:var(--color-log-header-fg);background:var(--color-log-header-bg);position:sticky;top:0;z-index:1;">${LOG_COLUMNS
    .filter((c) => visibleColKeys.has(c.key))
    .map((c) => html`<span class="log-${c.key}" data-col=${c.key}>${c.label}</span>`)}</div>`;
}

function renderPagination(totalRows: number, totalP: number): TemplateResult {
  if (totalRows === 0) return html``;
  const isFirst = state.logs.page <= 1; const isLast = state.logs.page >= totalP;
  return html`<div class="logs-pagination">
    <div class="mobile-pag-row-controls">
      <button ?disabled=${isFirst} @click=${() => logsGoPage(1)} title="First page" aria-label="First page">${icons.chevronsLeft()}</button>
      <button ?disabled=${isFirst} @click=${logsPrevPage} title="Previous page" aria-label="Previous page">${icons.chevronLeft()}<span class="pag-label">Prev</span></button>
      <span class="page-info">Page ${state.logs.page} of ${totalP}</span>
      <button ?disabled=${isLast} @click=${logsNextPage} title="Next page" aria-label="Next page"><span class="pag-label">Next</span>${icons.chevronRight()}</button>
      <button ?disabled=${isLast} @click=${() => logsGoPage(totalP)} title="Last page" aria-label="Last page">${icons.chevronsRight()}</button>
    </div>
    <div class="mobile-pag-row-meta">
      <span class="rows-info">${totalRows} row${totalRows !== 1 ? "s" : ""}</span>
      <label class="logs-follow-toggle"><input type="checkbox" id="logs-follow-input" ?checked=${state.logs.followTail} @change=${logsSetFollow}><span>Follow</span></label>
    </div>
  </div>`;
}

function renderLogsView(): TemplateResult {
  const allInflight = liveLogsStore.selectInflightRows();
  const sourceFinished = (state.logs.page > 1 && frozenLogsSnapshot) ? frozenLogsSnapshot : liveLogsStore.selectFinishedRows();
  const inflightRows = allInflight.filter(matchesLogFilter);
  const finishedRows = sourceFinished.filter(matchesLogFilter);
  const totalFinished = finishedRows.length;
  const rpp = state.logs.rowsPerPage;
  const totalP = Math.max(1, Math.ceil(totalFinished / rpp));
  if (state.logs.followTail) state.logs.page = 1; else if (state.logs.page > totalP) state.logs.page = totalP;
  if (state.logs.page < 1) state.logs.page = 1;
  const start = (state.logs.page - 1) * rpp;
  const pageFinishedRows = finishedRows.slice(start, Math.min(start + rpp, totalFinished));
  const visibleColKeys = state.logs.visibleColumns || new Set(LOG_COLUMNS.map((c) => c.key));
  const nowOffsetMs = clockStore.nowMs - liveLogsStore.clockOffsetMs;

  const activeApiKeyIds = liveLogsStore.selectActiveApiKeyIds();
  const hasNoApiKeyRows = liveLogsStore.hasNoApiKeyAttempts();
  const knownKeys = (state.apiKeys as Array<{ id: number; label?: string | null; key_prefix?: string | null }>) || [];
  const knownKeysMap = new Map(knownKeys.map((k) => [k.id, k]));
  const activeKeyOptions = Array.from(activeApiKeyIds)
    .sort((a, b) => a - b)
    .map((id) => {
      const k = knownKeysMap.get(id);
      const label = k?.label ? `${k.label} (#${id})` : (k?.key_prefix ? `${k.key_prefix}… (#${id})` : `#${id}`);
      return { id, label };
    });

  return html`
    <div class="logs-header">
      <div class="m-header-status-badges">
        <span id="logs-connection-status" class="logs-connection-badge disconnected" role="img" title="Disconnected" aria-label="Disconnected">
          <svg viewBox="0 0 16 16" width="10" height="10" aria-hidden="true" focusable="false"><circle cx="8" cy="8" r="7" fill="currentColor" /></svg>
        </span>
        <button id="logs-recording-toggle" class="logs-recording-toggle" type="button" @click=${() => void toggleRecording()}>
          <span class="logs-recording-dot" aria-hidden="true"></span>
          <span class="logs-recording-label">Record: <strong>OFF</strong></span>
        </button>
        <select class="logs-status-filter" aria-label="Filter by status" .value=${filterStatus} @change=${(e: Event) => onSetStatusFilter((e.target as HTMLSelectElement).value)}>
          <option value="all">All statuses</option>
          <option value="inflight">In-flight</option>
          <option value="2xx">2xx OK</option>
          <option value="4xx">4xx Client</option>
          <option value="5xx">5xx Server</option>
          <option value="error">All Errors</option>
        </select>
      </div>
      <div class="logs-header-actions m-header-actions-row">
        <button type="button" class="btn btn-sm ${isPaused ? "btn-warn" : ""}" title=${isPaused ? "Resume live updates" : "Pause live updates"} @click=${() => { isPaused = !isPaused; requestUpdate(); }}>
          ${isPaused ? icons.play() : icons.pause()}<span class="logs-action-label">${isPaused ? "Resume" : "Pause"}</span>
        </button>
        <button type="button" class="btn btn-sm" title="Export the visible rows as CSV" @click=${exportLogsCSV}>${icons.export()}<span class="logs-action-label">Export CSV</span></button>
        <div class="columns-menu-wrapper">
          <button id="logs-columns-toggle" type="button" class="logs-columns-toggle" title="Choose which columns to show" aria-haspopup="true" aria-expanded=${columnsMenuOpen ? "true" : "false"} @click=${() => toggleColumnsMenu()}>
            <span class="logs-action-label">Columns</span><span class="logs-columns-caret">${icons.caretDown()}</span>
          </button>
          <div class="columns-menu ${columnsMenuOpen ? "open" : ""}" role="menu">${LOG_COLUMNS.map((c) => html`<label class="columns-menu-item"><input type="checkbox" data-arg1="${c.key}" .checked=${live(visibleColKeys.has(c.key))} @change=${(e: Event) => { e.stopPropagation(); toggleColumn(c.key); }}><span>${c.label}</span></label>`)}</div>
        </div>
      </div>
    </div>
    <div class="logs-filter-toolbar">
      <div class="logs-search-box">
        <span class="logs-search-icon">${icons.search()}</span>
        <input type="search" class="logs-search-input" placeholder="Filter by model, provider, request id, trace..." .value=${filterSearch} @input=${onSearchInput} />
        ${filterSearch ? html`<button type="button" class="logs-search-clear" @click=${() => { if (searchDebounceTimer) clearTimeout(searchDebounceTimer); filterSearch = ""; frozenLogsSnapshot = null; state.logs.page = 1; state.logs.followTail = true; requestUpdate(); }}>${icons.close()}</button>` : null}
      </div>
      ${activeKeyOptions.length > 0 || hasNoApiKeyRows ? html`
        <select class="logs-key-filter" aria-label="Filter by API Key" .value=${filterApiKeyId} @change=${(e: Event) => onSetApiKeyFilter((e.target as HTMLSelectElement).value)}>
          <option value="all">All API Keys (${activeKeyOptions.length})</option>
          ${hasNoApiKeyRows ? html`<option value="none">No API Key</option>` : null}
          ${activeKeyOptions.map((k) => html`
            <option value=${String(k.id)}>${k.label}</option>
          `)}
        </select>
      ` : null}
    </div>
    <div class="logs" id="logs" @click=${(e: Event) => {
      const row = (e.target as Element | null)?.closest(".log-row[data-request-id]") as HTMLElement | null;
      if (!row) return;
      const id = row.dataset["id"]; const attemptKey = row.dataset["attemptKey"];
      const identity = id ? { kind: "row_id" as const, id: Number(id) } : (attemptKey ? { kind: "attempt" as const, attemptKey } : null);
      if (!identity) return;
      const clicked = liveLogsStore.selectDetail(identity);
      if (clicked) void openLogDetail(clicked.rowId ? String(clicked.rowId) : "", clicked.requestId, clicked.traceId, clicked);
    }}>
      <div class="logs-section logs-section-inflight ${inflightRows.length === 0 ? "empty" : ""}" id="logs-section-inflight">
        <div class="logs-section-header"><span class="logs-section-title"><span class="logs-inflight-dot"></span> Requests in progress</span><span class="logs-section-count">${inflightRows.length}</span></div>
        <div class="logs-scroll-area logs-scroll-area-inflight" id="logs-scroll-area-inflight">
          ${renderHeaderRow(visibleColKeys)}
          ${inflightRows.length === 0 ? html`<div class="empty empty-inflight logs-empty-placeholder" style="padding:1.5rem;text-align:center;color:var(--color-text-muted);">${filterSearch || filterStatus !== "all" ? "No in-progress requests matching filter." : "No requests in progress."}</div>` : repeat(inflightRows, (r) => r.attemptKey, (r) => html`<div data-key=${r.attemptKey}>${renderLogRowHtml(r, visibleColKeys, nowOffsetMs)}</div>`)}
        </div>
      </div>
      <div class="logs-section logs-section-finished" id="logs-section-finished">
        <div class="logs-section-header"><span class="logs-section-title">Finished or failed</span><span class="logs-section-count">${totalFinished}</span></div>
        <div class="logs-scroll-area logs-scroll-area-finished" id="logs-scroll-area-finished">
          ${renderHeaderRow(visibleColKeys)}
          ${pageFinishedRows.length === 0 ? html`<div class="empty empty-finished logs-empty-placeholder" style="padding:1.5rem;text-align:center;color:var(--color-text-muted);">${filterSearch || filterStatus !== "all" ? "No requests matching current filter." : "No recent requests yet."}</div>` : repeat(pageFinishedRows, (r) => r.attemptKey, (r) => html`<div data-key=${r.attemptKey}>${renderLogRowHtml(r, visibleColKeys, nowOffsetMs)}</div>`)}
        </div>
        ${renderPagination(totalFinished, totalP)}
      </div>
    </div>`;
}

export function logsPrevPage(): void {
  if (state.logs.page > 1) { state.logs.page--; if (state.logs.page === 1) { frozenLogsSnapshot = null; state.logs.followTail = true; } else state.logs.followTail = false; requestUpdate(); }
}
export function logsNextPage(): void {
  const totalP = Math.max(1, Math.ceil(liveLogsStore.selectFinishedRows().length / state.logs.rowsPerPage));
  if (state.logs.page < totalP) { if (state.logs.page === 1) frozenLogsSnapshot = [...liveLogsStore.selectFinishedRows()]; state.logs.page++; state.logs.followTail = false; requestUpdate(); }
}
export function logsGoPage(p: number): void {
  const totalP = Math.max(1, Math.ceil(liveLogsStore.selectFinishedRows().length / state.logs.rowsPerPage));
  const target = Math.max(1, Math.min(p, totalP));
  if (target === 1) { frozenLogsSnapshot = null; state.logs.followTail = true; } else { if (!frozenLogsSnapshot) frozenLogsSnapshot = [...liveLogsStore.selectFinishedRows()]; state.logs.followTail = false; }
  state.logs.page = target; requestUpdate();
}
export function logsSetFollow(e: Event): void {
  const enabled = !!((e.target as HTMLInputElement)?.checked);
  state.logs.followTail = enabled; if (enabled) { frozenLogsSnapshot = null; state.logs.page = 1; requestUpdate(); }
}
export function toggleColumnsMenu(): void {
  columnsMenuOpen = !columnsMenuOpen;
  requestUpdate();
  // The menu only exists in the DOM once lit-html has rendered the .open class,
  // which happens on the microtask requestUpdate just queued.
  if (columnsMenuOpen) requestAnimationFrame(clampColumnsMenu);
}

/**
 * The columns menu is anchored to the right edge of its button. That is correct
 * while the button sits at the right end of the header, but the menu is 160px
 * wide and a narrow tile has less room than that to the button's left, so it
 * crosses the tile edge and .tile-view clips it. Re-anchor it by hand once it is
 * laid out, and again on resize, so it always stays inside whatever actually
 * clips it.
 */
function clampColumnsMenu(): void {
  const menu = document.querySelector<HTMLElement>(".columns-menu.open");
  const wrap = menu?.parentElement;
  if (!menu || !wrap) return;
  menu.style.left = "";
  menu.style.right = "";
  // The first ancestor that clips is the binding boundary, not the viewport.
  let clipLeft = 0;
  let clipRight = window.innerWidth;
  for (let el = wrap.parentElement; el && el !== document.body; el = el.parentElement) {
    if (getComputedStyle(el).overflowX !== "visible") {
      const r = el.getBoundingClientRect();
      clipLeft = Math.max(clipLeft, r.left);
      clipRight = Math.min(clipRight, r.right);
      break;
    }
  }
  const wrapRect = wrap.getBoundingClientRect();
  const menuW = menu.offsetWidth;
  const margin = 8;
  // Preferred: flush with the button's right edge.
  let left = wrapRect.width - menuW;
  const absLeft = wrapRect.left + left;
  if (absLeft < clipLeft + margin) left += clipLeft + margin - absLeft;
  if (wrapRect.left + left + menuW > clipRight - margin) left -= wrapRect.left + left + menuW - (clipRight - margin);
  menu.style.left = `${Math.round(left)}px`;
  menu.style.right = "auto";
}
export function toggleColumn(key: string): void {
  if (!state.logs.visibleColumns) state.logs.visibleColumns = new Set(LOG_COLUMNS.map((c) => c.key));
  const set = state.logs.visibleColumns;
  if (set.has(key)) {
    if (set.size === 1) {
      // Re-render so the DOM checkbox reverts its checked state back to true
      requestUpdate();
      return;
    }
    set.delete(key);
  } else {
    set.add(key);
  }
  saveVisibleColumns(); requestUpdate();
}

export async function mountLogs(
  target?: HTMLElement | null | string,
  route?: string,
): Promise<(() => void) | void> {
  const container = (target instanceof HTMLElement) ? target : document.getElementById("main");
  if (!container) return;
  columnsMenuOpen = false;
  if (!state.logs.visibleColumns) state.logs.visibleColumns = loadVisibleColumns();
  state.logs.page = 1; state.logs.followTail = true; frozenLogsSnapshot = null;
  const cleanupReactive = mountView(container, renderLogsView);

  const onDocClickForMenu = (ev: Event): void => {
    if (!columnsMenuOpen || !(ev.target instanceof Element) || ev.target.closest(".columns-menu-wrapper")) return;
    columnsMenuOpen = false; requestUpdate();
  };
  document.addEventListener("click", onDocClickForMenu);
  const onResizeForMenu = (): void => { if (columnsMenuOpen) clampColumnsMenu(); };
  window.addEventListener("resize", onResizeForMenu);

  fetchRecordingState();
  if (!state.apiKeys || state.apiKeys.length === 0) {
    void api("/keys").then((keys) => {
      state.apiKeys = keys as typeof state.apiKeys;
      requestUpdate();
    }).catch(() => { /* non-fatal */ });
  }
  setMessageHandler(handleLogsMessage); connectLogsWebSocket();
  const onClockTick = () => {
    requestUpdate();
  };
  clockStore.subscribe(onClockTick);

  // E2E test helper: tests can trigger a view update after injecting rows directly into liveLogsStore
  (window as unknown as { __openproxyRequestUpdate?: () => void }).__openproxyRequestUpdate = requestUpdate;

  const rawRoute = route ?? location.hash ?? "";
  const qIdx = rawRoute.indexOf("?");
  if (qIdx >= 0) {
    const params = new URLSearchParams(rawRoute.slice(qIdx + 1));
    const traceId = params.get("trace_id") || ""; const requestId = params.get("request_id") || "";
    if (traceId || requestId) void openLogDetail("", requestId, traceId);
  }

  return () => {
    document.removeEventListener("click", onDocClickForMenu);
    window.removeEventListener("resize", onResizeForMenu);
    disconnectLogsWebSocket();
    clockStore.unsubscribe(onClockTick);
    cleanupReactive();
  };
}

setMessageHandler(handleLogsMessage);
