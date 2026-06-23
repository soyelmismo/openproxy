// views/logs.ts — live logs view (lit-html).
//
// MIGRATED to lit-html. The full table (header + body + pagination)
// is now a single TemplateResult; `requestUpdate()` triggers a
// microtask-coalesced re-render that diffs the new template against
// the previous one and patches only the changed DOM nodes. This
// replaces the old `logsEl.innerHTML = headerHtml + bodyHtml +
// paginationHtml` rebuild and the `requestAnimationFrame` render
// throttle (lit-html's microtask coalescing already does the same
// job).
//
// The 100ms latency ticker (`state/ticker.ts`) continues to mutate
// `.log-phase-sub` and `.log-latency` in place. Those elements are
// created by lit-html on first render with a static initial value
// (e.g. `0ms`); the ticker is the source of truth for the live
// counter. lit-html leaves them untouched on subsequent renders
// (their text content is a static part of the template, not a
// `${...}` interpolation), so the ticker's mutations survive
// `requestUpdate()` calls.
//
// Similarly, the connection-status badge (`#logs-connection-status`)
// and the recording-toggle button (`#logs-recording-toggle`) are
// rendered with STATIC class/text in the template; their dynamic
// state is managed by direct DOM manipulation in `state/ws.ts`
// (`setLogsStatus`) and `components/recording-toggle.ts`
// (`renderRecordingToggle`). lit-html leaves them alone after the
// first render, so the direct mutations survive.
//
// The detail modal lives in `components/log-detail.ts` to keep this
// file focused on orchestration.

import { html, type TemplateResult } from "lit-html";
// unsafeHTML import removed — log-row.ts now returns TemplateResult directly.
import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { renderLogRowHtml } from "../components/log-row.js";
import { LOG_COLUMNS, LOGS_VISIBLE_COLUMNS_STORAGE_KEY } from "../lib/constants.js";
import {
  connectLogsWebSocket,
  setMessageHandler,
  disconnectLogsWebSocket,
} from "../state/ws.js";
import { startLogLatencyTicker, stopLogLatencyTicker } from "../state/ticker.js";
import { fetchRecordingState, toggleRecording } from "../components/recording-toggle.js";
import { showToast } from "../components/toast.js";
import { mountView, requestUpdate } from "../state/reactive.js";
import {
  showLogDetail,
  updateOpenLogDetail,
  hasCompleteLogDetail,
} from "../components/log-detail.js";
import type { RecentUsageRow, StageEvent } from "../lib/types/api.js";

// Local copy of the LogDetailLog shape from components/log-detail.ts.
// G3 kept that interface private (it's an "open record" union of
// RecentUsageRow + the detail-endpoint extras). We re-declare the
// minimum fields we touch — `id`, `request_id` — so the call sites
// (showLogDetail / updateOpenLogDetail / hasCompleteLogDetail) accept
// a RecentUsageRow without forcing us to export the shape from the
// component. The component itself accepts the same open shape.
interface LogDetailLog {
  id?: number;
  request_id?: string;
  [k: string]: unknown;
}

// WebSocket message envelope. The server sends one of four
// payload types — see `handleLogsMessage` below for the
// per-branch handling. We model the discriminated union on
// `type` and let the per-branch data be `unknown` so the
// consumers have to type-guard before reading it.
interface WsEnvelope {
  type: string;
  data?: unknown;
  row?: unknown;
  rows?: unknown;
  message?: string;
  request_id?: string;
  delta?: string;
  complete?: boolean;
  id?: number;
  // H7 fix: `lag_warning` and `resync` envelopes
  // (RACE-F-5) carry extra fields the dashboard uses to
  // recover from a lagged broadcast channel. The plan said
  // we MUST NOT change the wire contract of WS envelopes,
  // so these are additional fields on an extensible
  // JSON object — old clients that don't know about them
  // simply ignore the unknown keys. We mark both as
  // optional so existing usage of `WsEnvelope` keeps
  // type-checking.
  skipped?: number;
  since_id?: number;
}

// ---- Module-local UI state ----------------------------------------------
//
// `columnsMenuOpen` tracks whether the columns popover menu is open.
// Toggling it and calling `requestUpdate()` re-renders the menu with
// the new `.open` class. The document-level outside-click handler
// (installed once per session in `mountLogs`) reads this flag to
// close the menu when the user clicks elsewhere.
let columnsMenuOpen: boolean = false;

function totalPages(): number {
  return Math.max(1, Math.ceil(state.logs.rows.length / state.logs.rowsPerPage));
}

// ---- Visible-columns state ---------------------------------------------
// The user can hide any subset of the log-table columns. The set of
// visible column keys lives on `state.logs.visibleColumns` (a Set),
// and is persisted to localStorage as a JSON array. Default is
// "all columns visible"; an empty set is forbidden (you can't hide
// the last column).
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
    // Corrupt localStorage value — fall back to "all visible".
    result = new Set(allKeys);
  }
  return result;
}

function saveVisibleColumns(): void {
  // `state.logs.visibleColumns` is `Set<string> | null`. The narrow
  // in the `if (!cols) return;` above doesn't propagate to the
  // iterator context (TS widens the type back to the union when
  // reaching the spread), so we re-narrow to a concrete Set here.
  const cols: Set<string> | null = state.logs.visibleColumns;
  if (!cols) return;
  try {
    localStorage.setItem(
      LOGS_VISIBLE_COLUMNS_STORAGE_KEY,
      JSON.stringify(Array.from(cols)),
    );
  } catch (_e) { /* localStorage may be disabled — non-fatal */ }
}

function mergeLogsByDescId(existing: RecentUsageRow[], incoming: RecentUsageRow[]): RecentUsageRow[] {
  const merged = new Map<string | number, RecentUsageRow>();
  for (const row of existing) {
    const k = Number(row.id) || row.id;
    merged.set(k, row);
    if (row.request_id) merged.set("req:" + row.request_id, row);
  }
  for (const row of incoming) {
    if (row == null || row.id == null) continue;
    const k = Number(row.id) || row.id;
    let base = merged.get(k);
    if ((!k || k === 0) && row.request_id) {
      const reqKey = "req:" + row.request_id;
      if (merged.has(reqKey)) base = merged.get(reqKey) as RecentUsageRow;
    }
    merged.set(k, { ...(base || {}), ...row });
    if (row.request_id) merged.set("req:" + row.request_id, merged.get(k) as RecentUsageRow);
    state.logs.lastSeenId = Math.max(state.logs.lastSeenId, row.id);
  }
  const seenKeys = new Set<string | number | symbol>();
  const result = Array.from(merged.values()).filter((r) => {
    const key = r.id != null ? Number(r.id) : (r.request_id ? "r:" + r.request_id : Symbol());
    if (typeof key === "symbol" || seenKeys.has(key)) return false;
    seenKeys.add(key);
    return true;
  }).sort((a, b) => (b.id || 0) - (a.id || 0));
  const limit = state.logs.maxRows;
  if (result.length > limit) {
    const removed = result.slice(limit);
    const finalResult = result.slice(0, limit);
    for (const r of removed) {
      const k = Number(r.id) || r.id;
      merged.delete(k);
    }
    state.logs.rowById = merged as Map<number, RecentUsageRow>;
    return finalResult;
  }
  state.logs.rowById = merged as Map<number, RecentUsageRow>;
  return result;
}

// ---- Templates ---------------------------------------------------------

function renderHeaderRow(visibleColKeys: Set<string>): TemplateResult {
  // The header row uses the same `.log-row` class as the body rows
  // (the CSS targets both). Inline styles preserve the original
  // look (sticky, uppercase, muted) without needing a new CSS class.
  return html`<div class="log-row" style="cursor:default;border-bottom:1px solid var(--color-border);font-weight:600;font-size:0.72rem;text-transform:uppercase;color:var(--color-text-muted);background:var(--color-log-header-bg);position:sticky;top:0;z-index:1;">${LOG_COLUMNS
    .filter((c) => visibleColKeys.has(c.key))
    .map((c) => html`<span class="log-${c.key}" data-col=${c.key}>${c.label}</span>`)}</div>`;
}

// Render a single log row. `renderLogRowHtml` (in components/log-row.ts)
// returns an HTML string — that component is not yet migrated to
// lit-html. We embed it via `unsafeHTML`. The string is built
// entirely from server-controlled data with `escapeHtml`/`escapeAttr`
// applied at every interpolation inside `log-row.ts`, so unsafe
// injection is safe here. When `log-row.ts` is migrated to return a
// `TemplateResult`, the `unsafeHTML` wrapper can be dropped.
function renderLogRow(
  r: RecentUsageRow & { __inflight?: boolean },
  visibleColKeys: Set<string>,
): TemplateResult {
  // Resolve the live stage for this row. Primary key is
  // `trace_id` so each attempt of a multi-attempt request
  // (per-target retry, fallback to next combo target, race
  // loser) has its own phase. The request_id fallback is
  // only for the edge case where a row's `trace_id` is
  // empty (synthetic events emitted from the frontend
  // itself).
  //
  // CRITICAL: for finalized rows (status_code > 0), derive
  // the stage from the row's own status_code instead of
  // looking up the shared stage map. When a request is
  // retried, trace_id is reused across attempts, so the
  // stage map only holds one entry — the retry's
  // "completed" would overwrite the failed attempt's
  // "failed", causing the failed row to show "completado".
  let stage: StageEvent | undefined;
  if (r.status_code > 0) {
    const hasError = !!(r.error_message && r.error_message.length > 0);
    stage = {
      request_id: r.request_id,
      trace_id: r.trace_id,
      provider_id: r.provider_id,
      upstream_model_id: r.upstream_model_id,
      stage: r.race_lost ? "cancelled" : ((r.status_code >= 400 || hasError) ? "failed" : "completed"),
      elapsed_ms: r.total_ms || 0,
      connect_ms: r.connect_ms,
      ttft_ms: r.ttft_ms,
      status_code: r.status_code,
      error: r.error_message ?? null,
      stop_reason: r.stop_reason ?? null,
      compression_savings_pct: r.compression_savings_pct ?? null,
      compression_techniques: r.compression_techniques ?? null,
      timestamp: r.created_at || new Date().toISOString(),
    };
  } else {
    stage =
      (r.trace_id && state.logs.stagesByTraceId.get(r.trace_id)) ||
      (r.request_id && state.logs.stagesByRequestId.get(r.request_id)) ||
      undefined;
  }
  return html`${renderLogRowHtml(r, stage, visibleColKeys, r.total_ms)}`;
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
    <label class="logs-follow-toggle" title="When ON, new rows automatically scroll the view to the most recent page. When OFF, the view stays on the page you are reading.">
      <input type="checkbox" id="logs-follow-input" ?checked=${state.logs.followTail} @change=${logsSetFollow}>
      <span>Follow</span>
    </label>
  </div>`;
}

function renderColumnsMenu(): TemplateResult {
  const visible = state.logs.visibleColumns || new Set(LOG_COLUMNS.map((c) => c.key));
  return html`<div class="columns-menu ${columnsMenuOpen ? "open" : ""}" role="menu">${LOG_COLUMNS.map((c) => html`<label class="columns-menu-item"><input type="checkbox" ?checked=${visible.has(c.key)} @change=${(e: Event) => onColumnToggle(c.key, e)}><span>${c.label}</span></label>`)}</div>`;
}

function renderLogsView(): TemplateResult {
  // Build the merged row list: historical rows + in-flight
  // placeholders. The inflight rows get a synthetic id
  // (MAX_SAFE_INTEGER - created_at_ms) so they sort to the top of
  // the descending-id list (newest first) without colliding with
  // real DB ids.
  const inflightRows: (RecentUsageRow & { __inflight: boolean })[] = [
    ...Array.from(state.logs.inflightByTraceId.values()),
    ...Array.from(state.logs.inflightByRequestId.values()),
  ].map((p) => {
    const t = Date.parse(p.created_at);
    const syntheticId = isFinite(t) ? (Number.MAX_SAFE_INTEGER - t) : Number.MAX_SAFE_INTEGER;
    return Object.assign({}, p, { id: syntheticId, __inflight: true });
  });
  const rows = (state.logs.rows as (RecentUsageRow & { __inflight?: boolean })[])
    .concat(inflightRows)
    .sort((a, b) => (b.id || 0) - (a.id || 0));
  const totalRows = rows.length;
  const rpp = state.logs.rowsPerPage;
  const totalP = Math.max(1, Math.ceil(totalRows / rpp));
  if (state.logs.page > totalP) state.logs.page = totalP;
  if (state.logs.page < 1) state.logs.page = 1;
  const start = (state.logs.page - 1) * rpp;
  const end = Math.min(start + rpp, totalRows);
  const pageRows = rows.slice(start, end);
  const visibleColKeys = state.logs.visibleColumns || new Set(LOG_COLUMNS.map((c) => c.key));

  // The connection-status badge and the recording-toggle button are
  // rendered with STATIC class/text content. Their dynamic state is
  // managed by direct DOM manipulation in `state/ws.ts`
  // (`setLogsStatus`) and `components/recording-toggle.ts`
  // (`renderRecordingToggle`). lit-html leaves static attributes and
  // static text alone after the first render, so those direct
  // mutations survive `requestUpdate()`. Do NOT add `${...}`
  // interpolations to these elements without also routing the
  // updates through `requestUpdate()`.
  return html`
    <div class="logs-header">
      <h2>Live Logs</h2>
      <div class="logs-header-actions">
        <div class="columns-menu-wrapper">
          <button id="logs-columns-toggle" type="button" class="logs-columns-toggle" aria-haspopup="true" aria-expanded=${columnsMenuOpen ? "true" : "false"} title="Choose which columns to show or hide. The selection is saved in this browser." @click=${onToggleColumnsMenu}>
            <span>Columns</span>
            <span class="logs-columns-caret" aria-hidden="true">▾</span>
          </button>
          ${renderColumnsMenu()}
        </div>
        <span id="logs-connection-status" class="logs-connection-badge disconnected">🔴 disconnected</span>
        <button id="logs-recording-toggle" class="logs-recording-toggle" type="button" aria-pressed="false" title="When ON, the server saves full request/response bodies and headers for every request (disk). When OFF, only metadata is kept." @click=${onRecordingToggleClick}>
          <span class="logs-recording-dot" aria-hidden="true"></span>
          <span class="logs-recording-label">⏺ Record: <strong>OFF</strong></span>
        </button>
      </div>
    </div>
    <div class="logs" id="logs" @click=${onLogsClick}>
      ${renderHeaderRow(visibleColKeys)}
      ${pageRows.length === 0
        ? html`<div class="empty" style="padding:2rem;">No recent requests yet. Use the API to see logs appear here in real time.</div>`
        : pageRows.map((r) => renderLogRow(r, visibleColKeys))}
      ${renderPagination(totalRows, totalP)}
    </div>
  `;
}

// ---- Click handlers (replaces data-action + attachLogRowHandlers) ------

// Event delegation for log-row clicks. A single `@click` handler on
// the `#logs` container reads the closest `.log-row[data-request-id]`
// ancestor of the click target and opens the detail modal. This
// replaces the per-row `addEventListener` in `attachLogRowHandlers`
// (which had to be re-run after every `innerHTML` rebuild).
function onLogsClick(e: Event): void {
  const target = e.target;
  if (!(target instanceof Element)) return;
  // The header row also has class `.log-row` but no `data-request-id`;
  // the attribute selector skips it.
  const rowEl = target.closest(".log-row[data-request-id]");
  if (!rowEl) return;
  const el = rowEl as HTMLElement;
  const id = el.dataset["id"] || "";
  const requestId = el.dataset["requestId"] || "";
  const traceId = el.dataset["traceId"] || "";
  if (!id && !requestId) return;
  void openLogDetail(id, requestId, traceId);
}

function onToggleColumnsMenu(): void {
  columnsMenuOpen = !columnsMenuOpen;
  requestUpdate();
}

function onColumnToggle(key: string, e: Event): void {
  // Stop the click from bubbling to the document-level outside-click
  // handler (which would close the menu before the @change could fire
  // on the next render).
  e.stopPropagation();
  toggleColumn(key);
}

function onRecordingToggleClick(): void {
  void toggleRecording();
}

// ---- Pagination handlers -----------------------------------------------
//
// Exported (and re-exported via the registry in handlers/registry.ts)
// for back-compat with anything that still routes through
// `data-action`. The lit-html template uses direct `@click` handlers
// instead, so the registry entries are unused at runtime but kept
// for type-safety.

export function logsPrevPage(): void {
  if (state.logs.page > 1) {
    state.logs.page--;
    if (state.logs.page === 1) state.logs.followTail = true;
    requestUpdate();
  }
}
export function logsNextPage(): void {
  if (state.logs.page < totalPages()) {
    state.logs.page++;
    if (state.logs.page >= totalPages()) state.logs.followTail = false;
    requestUpdate();
  }
}
export function logsGoPage(p: number): void {
  const total = totalPages();
  state.logs.page = Math.max(1, Math.min(p, total));
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

// ---- Columns menu ------------------------------------------------------
//
// `toggleColumnsMenu` and `toggleColumn` are exported for back-compat
// with the registry. The lit-html template uses direct `@click` /
// `@change` handlers (`onToggleColumnsMenu`, `onColumnToggle`).

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
    // Refuse to hide the last visible column — an empty table is
    // useless and the user has no "show all" affordance without
    // toggling each one back on. The browser's default click has
    // already flipped the checkbox to unchecked; `requestUpdate()`
    // re-renders and lit-html sets `?checked` back to true (because
    // `set.has(key)` is still true).
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

// ---- Stage event handling ----------------------------------------------

function handleStageEvent(event: StageEvent): void {
  if (!event || !event.request_id) return;
  const requestId = event.request_id;
  // Index the live stage by `trace_id` (per attempt), not by
  // `request_id`. A request with retries has multiple `trace_id`s
  // — keying by `request_id` would overwrite the stage of every
  // previous attempt of that request, which is the user-visible
  // "phase of the failed attempt got rewritten to 'Started' on
  // retry" bug. The request_id-keyed map is the fallback for the
  // rare case where the event has no `trace_id` (we keep both
  // maps in sync via `setStage`, but only `stagesByTraceId` is
  // read by the renderer).
  setStage(event, requestId);
  // Find an existing row in the rendered list that matches this
  // event's `(request_id, trace_id)`. A matching row is either:
  //   * a row whose `trace_id` equals the event's — the normal
  //     case for a fresh request or a retry whose row already
  //     arrived, or
  //   * a historical row (status_code > 0) for the same
  //     `request_id` but with a *different* `trace_id` — this is
  //     the retry case the user reported: the old row stays
  //     visible with its failed/completed stage, and we don't
  //     want to mutate it. We also don't want to bind the new
  //     event to it (its phase would be misleading).
  const traceId = event.trace_id || "";
  const exactRow = traceId
    ? state.logs.rows.find((r) => r.request_id === requestId && r.trace_id === traceId)
    : undefined;
  if (exactRow) {
    if (exactRow.id != null) state.logs.rowById.set(exactRow.id, exactRow);
    if (state.logs.followTail) state.logs.page = 1;
    requestUpdate();
    updateOpenLogDetail(exactRow as unknown as LogDetailLog);
    return;
  }
  // A row exists for this `request_id` but with a different
  // `trace_id` (retry against a fresh trace_id): the new attempt
  // gets its own inflight placeholder, leaving the historical row
  // untouched.
  if (traceId && !state.logs.inflightByTraceId.has(traceId)) {
    state.logs.inflightByTraceId.set(traceId, {
      id: 0,
      request_id: requestId,
      provider_id: event.provider_id || "",
      upstream_model_id: event.upstream_model_id || "",
      created_at: event.timestamp || new Date().toISOString(),
      status_code: 0, prompt_tokens: null, completion_tokens: null,
      total_ms: 0, cost_usd: 0, is_streaming: false, stream_complete: false, race_lost: false,
      trace_id: traceId,
      connect_ms: null,
      ttft_ms: null,
      request_body_json: null,
      response_body_json: null,
      // RecentUsageRow has these as required-with-null. For an
      // inflight placeholder we don't have the headers yet, so they
      // stay null. Same for race_* (single-target rows race_total
      // is null until the proxy finishes counting attempts).
      request_headers: null,
      response_headers: null,
      race_total: null,
      race_attempts: null,
      error_message: null,
      stop_reason: null,
      compression_savings_pct: null,
      compression_techniques: null,
    });
  } else if (traceId && state.logs.inflightByTraceId.has(traceId)) {
    // Update existing inflight placeholder with new stage event
    // data (model, provider, streaming status). The `started`
    // event arrives with an empty upstream_model_id; later
    // `connecting` / `streaming` events carry the real model.
    const existing = state.logs.inflightByTraceId.get(traceId)!;
    if (event.upstream_model_id) existing.upstream_model_id = event.upstream_model_id;
    if (event.provider_id) existing.provider_id = event.provider_id;
    if (event.stage === "streaming") existing.is_streaming = true;
    if (event.status_code > 0) existing.status_code = event.status_code;
  } else if (!traceId && !state.logs.inflightByRequestId.has(requestId)) {
    // Trace_id-less event: fall back to the request_id-keyed
    // inflight map (and the request_id-keyed stage map, handled
    // by setStage above). This branch is only reachable for
    // synthetic events emitted from the frontend itself.
    state.logs.inflightByRequestId.set(requestId, {
      id: 0,
      request_id: requestId,
      provider_id: event.provider_id || "",
      upstream_model_id: event.upstream_model_id || "",
      created_at: event.timestamp || new Date().toISOString(),
      status_code: 0, prompt_tokens: null, completion_tokens: null,
      total_ms: 0, cost_usd: 0, is_streaming: false, stream_complete: false, race_lost: false,
      trace_id: "",
      connect_ms: null,
      ttft_ms: null,
      request_body_json: null,
      response_body_json: null,
      request_headers: null,
      response_headers: null,
      race_total: null,
      race_attempts: null,
      error_message: null,
      stop_reason: null,
      compression_savings_pct: null,
      compression_techniques: null,
    });
  } else if (!traceId && state.logs.inflightByRequestId.has(requestId)) {
    const existing = state.logs.inflightByRequestId.get(requestId)!;
    if (event.upstream_model_id) existing.upstream_model_id = event.upstream_model_id;
    if (event.provider_id) existing.provider_id = event.provider_id;
    if (event.stage === "streaming") existing.is_streaming = true;
    if (event.status_code > 0) existing.status_code = event.status_code;
  }
  if (state.logs.followTail) state.logs.page = 1;
  requestUpdate();
}

// Mirror the stage event into the two stage maps keyed by
// `trace_id` (primary) and `request_id` (fallback for events
// with an empty `trace_id`). Centralized so callers can't forget
// to update one of the two.
function setStage(event: StageEvent, requestId: string): void {
  const traceId = event.trace_id || "";
  // Terminal events ("completed" / "failed" / "cancelled") are sticky — a late
  // non-terminal event (e.g. a reordered "streaming" broadcast, or a "connecting"
  // arriving after a synthesized "cancelled" from the reaper) must not clobber a
  // terminal that's already in the map.
  const isTerminal = (s: string): boolean =>
    s === "completed" || s === "failed" || s === "cancelled";
  const map = traceId ? state.logs.stagesByTraceId : state.logs.stagesByRequestId;
  const key = traceId || requestId;
  const existing = map.get(key);
  if (existing && isTerminal(existing.stage) && !isTerminal(event.stage)) {
    return;
  }
  map.set(key, event);
}

// Synthesize a terminal stage event from a finalized usage row
// when the backend's terminal event was missed (lagged subscriber,
// resync, history, or recording=OFF). Only emits when the
// currently-stored stage is still non-terminal.
function synthesizeTerminalEvent(row: RecentUsageRow): void {
  if (!row.request_id) return;
  if (row.status_code <= 0) return;
  // Look up the currently-stored stage for this attempt.
  const existingStage: StageEvent | undefined = row.trace_id
    ? state.logs.stagesByTraceId.get(row.trace_id)
    : state.logs.stagesByRequestId.get(row.request_id);
  const existingIsTerminal: boolean = !!existingStage &&
    (existingStage.stage === "completed" || existingStage.stage === "failed");
  if (existingIsTerminal) return;
  // If error_message is set, the request actually failed regardless of
  // status_code. This covers edge cases where the backend recorded a
  // partial 2xx status (e.g. timeout after headers received) but the
  // request didn't actually complete.
  const hasError = !!(row.error_message && row.error_message.length > 0);
  const synth: StageEvent = {
    request_id: row.request_id,
    stage: (row.status_code >= 400 || hasError) ? "failed" : "completed",
    elapsed_ms: row.total_ms || 0,
    status_code: row.status_code,
    timestamp: row.created_at || new Date().toISOString(),
    trace_id: row.trace_id,
    provider_id: row.provider_id,
    upstream_model_id: row.upstream_model_id,
    connect_ms: row.connect_ms,
    ttft_ms: row.ttft_ms,
    error: row.error_message ?? null,
    stop_reason: row.stop_reason ?? null,
    compression_savings_pct: row.compression_savings_pct ?? null,
    compression_techniques: row.compression_techniques ?? null,
  };
  setStage(synth, row.request_id);
}

// Race-aware fast reaping. When a race winner's row arrives, any
// sibling inflight placeholders for the same `request_id` (but a
// different `trace_id`) that are still in a non-terminal stage are
// race losers whose terminal "cancelled" event was lost (broadcast
// lag) or whose task was aborted before recording. Synthesize a
// terminal "cancelled" so they don't linger as ghosts. Safe: once a
// winner is found every sibling is a loser; a later real "cancelled"
// event or `row` just updates / cleans up the placeholder.
function reapRaceLosers(winnerRow: RecentUsageRow): void {
  const rid = winnerRow.request_id;
  if (!rid) return;
  const isTerminal = (s: string | undefined): boolean =>
    !!s && (s === "completed" || s === "failed" || s === "cancelled");
  for (const [tid, placeholder] of state.logs.inflightByTraceId) {
    if (tid === winnerRow.trace_id) continue;
    if (placeholder.request_id !== rid) continue;
    const stage = state.logs.stagesByTraceId.get(tid);
    if (stage && isTerminal(stage.stage)) continue;
    const synth: StageEvent = {
      request_id: rid,
      trace_id: tid,
      provider_id: placeholder.provider_id,
      upstream_model_id: placeholder.upstream_model_id,
      stage: "cancelled",
      elapsed_ms: 0,
      connect_ms: null,
      ttft_ms: null,
      status_code: 499,
      error: "race lost",
      stop_reason: null,
      compression_savings_pct: null,
      compression_techniques: null,
      timestamp: new Date().toISOString(),
    };
    setStage(synth, rid);
  }
}

// Stale inflight reaper (fallback). Scans inflight placeholders whose
// stage is still non-terminal and whose `created_at` is older than the
// threshold (well beyond any upstream timeout / watchdog). These are
// ghosts left by a lost terminal event AND a dropped/missing usage row
// (e.g. broadcast lag where resync brought no row because the row was
// never written). Synthesize a terminal "cancelled" so the entry stops
// ticking and is visually resolved. Conservative threshold to avoid
// reaping genuinely slow in-flight requests.
const STALE_INFLIGHT_MS = 120_000;
function reapStaleInflight(): void {
  const now = Date.now();
  const isTerminal = (s: string | undefined): boolean =>
    !!s && (s === "completed" || s === "failed" || s === "cancelled");
  let reaped = false;
  const scan = (map: Map<string, RecentUsageRow>, byTrace: boolean): void => {
    for (const [key, placeholder] of map) {
      const stage = byTrace
        ? state.logs.stagesByTraceId.get(key)
        : state.logs.stagesByRequestId.get(key);
      if (stage && isTerminal(stage.stage)) continue;
      const t = Date.parse(placeholder.created_at || "");
      if (!isFinite(t) || now - t < STALE_INFLIGHT_MS) continue;
      const synth: StageEvent = {
        request_id: placeholder.request_id,
        trace_id: byTrace ? key : "",
        provider_id: placeholder.provider_id,
        upstream_model_id: placeholder.upstream_model_id,
        stage: "cancelled",
        elapsed_ms: 0,
        connect_ms: null,
        ttft_ms: null,
        status_code: 499,
        error: "cancelled (stale)",
        stop_reason: null,
        compression_savings_pct: null,
        compression_techniques: null,
        timestamp: new Date().toISOString(),
      };
      setStage(synth, placeholder.request_id);
      // Bug fix: also update the placeholder's status_code and
      // error_message so the table and modal show consistent state.
      // Previously the placeholder kept its old status_code (200 from
      // the "streaming" stage) while the stage map said "cancelled /
      // 499" — the table showed "FALLÓ" (from the stage) but the
      // modal showed "200" (from the placeholder's status_code).
      placeholder.status_code = 499;
      placeholder.error_message = "Stream stalled — no response from upstream for 120s. The request was cancelled by the stale-inflight reaper.";
      reaped = true;
    }
  };
  scan(state.logs.inflightByTraceId, true);
  scan(state.logs.inflightByRequestId, false);
  // Only re-render when something actually changed — avoids a full
  // table re-render every tick while ordinary inflight entries exist.
  if (reaped) requestUpdate();
}

export function startStaleInflightReaper(): void {
  if (state.logs.staleReaperHandle) return;
  state.logs.staleReaperHandle = setInterval(reapStaleInflight, 5_000);
}

export function stopStaleInflightReaper(): void {
  if (state.logs.staleReaperHandle) {
    clearInterval(state.logs.staleReaperHandle);
    state.logs.staleReaperHandle = null;
  }
}

// H7 fix: when the server reports it lost us on a
// broadcast channel, it sends a `{"type":"resync",
// "since_id":N}` envelope. We then fetch any rows newer than
// N from the `usage/recent` endpoint and merge them in.
// Without this, a slow dashboard would permanently lose the
// rows it failed to consume in time.
async function resyncUsageRows(sinceId: number): Promise<void> {
  try {
    const rows = await api(
      `/usage/recent?since_id=${encodeURIComponent(String(sinceId))}&limit=500`,
    ) as RecentUsageRow[] | null;
    if (Array.isArray(rows) && rows.length > 0) {
      state.logs.rows = mergeLogsByDescId(state.logs.rows, rows);
      for (const r of rows) {
        if (r && typeof r.id === "number") {
          state.logs.rowById.set(r.id, r);
        }
        // Clear inflight placeholders for resynced rows.
        if (r.request_id && state.logs.inflightByRequestId.has(r.request_id)) {
          state.logs.inflightByRequestId.delete(r.request_id);
        }
        if (r.trace_id && state.logs.inflightByTraceId.has(r.trace_id)) {
          state.logs.inflightByTraceId.delete(r.trace_id);
        }
        // Synthesize terminal events for finished rows so the
        // ticker doesn't keep growing on resynced data.
        synthesizeTerminalEvent(r);
      }
      if (state.logs.followTail) state.logs.page = 1;
      requestUpdate();
    }
  } catch (e: unknown) {
    const err = e instanceof Error ? e : null;
    const msg = err ? err.message : String(e);
    showToast(`Failed to refetch missed log rows: ${msg}`, "error");
  }
}

function isStageEventShape(x: unknown): x is StageEvent {
  if (!x || typeof x !== "object") return false;
  const o = x as Record<string, unknown>;
  return typeof o["request_id"] === "string" && typeof o["stage"] === "string";
}

function isRecentUsageRowShape(x: unknown): x is RecentUsageRow {
  if (!x || typeof x !== "object") return false;
  const o = x as Record<string, unknown>;
  // The recent-usage row always has a `request_id` and a
  // `created_at`. Other fields can be present or absent, but
  // these two are stable across the long-poll and detail paths.
  return typeof o["request_id"] === "string" && typeof o["created_at"] === "string";
}

function handleLogsMessage(raw: MessageEvent): void {
  let msg: WsEnvelope;
  try {
    const parsed: unknown = JSON.parse(raw.data);
    msg = parsed as WsEnvelope;
  } catch (_e) {
    showToast("Live Logs received an invalid WebSocket message.", "error");
    return;
  }
  if (typeof window !== "undefined") {
    const w = window as unknown as { __logMsgTrace?: { t: number; type: string; hasData: boolean; hasRow: boolean; keys: string[] }[] };
    w.__logMsgTrace = w.__logMsgTrace || [];
    const keys = msg && typeof msg === "object" ? Object.keys(msg).slice(0, 10) : [];
    w.__logMsgTrace.push({ t: Date.now(), type: String(msg?.type ?? ""), hasData: !!msg?.data, hasRow: !!msg?.row, keys });
  }
  if (msg.type === "history") {
    const rawRows = Array.isArray(msg.rows) ? msg.rows : [];
    const rows: RecentUsageRow[] = rawRows.filter(isRecentUsageRowShape);
    state.logs.rows = mergeLogsByDescId(state.logs.rows, rows);
    // Historical rows are by definition finished (they came from
    // the DB, not a live stream). Synthesize terminal events so
    // the latency ticker doesn't keep ticking on them.
    for (const r of rows) {
      synthesizeTerminalEvent(r);
    }
    state.logs.page = 1; state.logs.followTail = true; requestUpdate();
  } else if (msg.type === "row") {
    const candidate = msg.data ?? msg.row ?? msg;
    if (!isRecentUsageRowShape(candidate)) return;
    const row = candidate;
    state.logs.rows = mergeLogsByDescId(state.logs.rows, [row]);
    if (row.request_id && state.logs.inflightByRequestId.has(row.request_id)) {
      state.logs.inflightByRequestId.delete(row.request_id);
    }
    if (row.trace_id && state.logs.inflightByTraceId.has(row.trace_id)) {
      // Drop the per-attempt inflight placeholder once the row
      // arrives. The row carries the same `trace_id` as the
      // placeholder (set by `handleStageEvent`), so this lookup
      // is well-defined even when retries are active.
      state.logs.inflightByTraceId.delete(row.trace_id);
    }
    if (row.is_streaming && !row.stream_complete) {
      const tokensMap = state.logs.liveTokens as unknown as Map<string, string>;
      tokensMap.set(row.request_id, tokensMap.get(row.request_id) || "");
    }
    // Synthesize a terminal stage event if the backend's
    // terminal event was missed (lagged subscriber, recording=OFF,
    // or streaming without [DONE]).
    synthesizeTerminalEvent(row);
    // Race-aware fast reaping: when a race winner's row arrives,
    // any sibling inflight placeholders for the same request_id
    // (different trace_id) still in a non-terminal stage are race
    // losers whose terminal "cancelled" event was lost. Synthesize
    // "cancelled" so they don't linger as ghosts stuck at
    // "connecting"/"started".
    if (row.race_total && row.race_total > 1) {
      reapRaceLosers(row);
    }
    if (state.logs.followTail) state.logs.page = 1;
    requestUpdate();
    updateOpenLogDetail(row as unknown as LogDetailLog);
  } else if (msg.type === "stage") {
    const candidate = msg.data ?? msg;
    if (isStageEventShape(candidate)) {
      handleStageEvent(candidate);
    }
  } else if (msg.type === "error") {
    showToast(msg.message || "Live Logs WebSocket error", "error");
  } else if (msg.type === "lag_warning") {
    // H7 fix: the server detected a broadcast `Lagged(_)` on
    // either the row or the stage channel. A `resync` envelope
    // follows immediately (handled below). Show a persistent
    // banner so the operator knows the displayed log is not
    // a complete picture; the resync fetch will fill in the
    // gap in the background.
    const skipped = Number(msg.skipped || 0);
    showToast(
      `Live Logs broadcast lagged; ${skipped} event(s) skipped. ` +
        `Refetching to catch up…`,
      "warning",
    );
  } else if (msg.type === "resync") {
    // H7 fix: the server lost us on a broadcast channel and
    // is asking the dashboard to fetch any rows newer than
    // `since_id` to recover. This is the only path that
    // prevents permanent state loss for slow dashboards.
    const sinceId = Number(msg.since_id || 0);
    void resyncUsageRows(sinceId);
  }
}

async function openLogDetail(id: string, requestId: string, traceId?: string): Promise<void> {
  const numericId = Number(id);
  // Prefer the inflight placeholder (by trace_id) when this is an
  // in-flight / ghost entry — its real `id` is 0 and there is no DB
  // row to fetch. Synthetic ids (Number.MAX_SAFE_INTEGER - ts) are
  // huge; real DB ids are small auto-increments, so a large numericId
  // also signals an inflight/ghost entry.
  const inflight: RecentUsageRow | undefined =
    (traceId ? state.logs.inflightByTraceId.get(traceId) : undefined) ||
    (requestId ? state.logs.inflightByRequestId.get(requestId) : undefined);
  const isSyntheticId: boolean =
    Number.isFinite(numericId) && numericId > 1_000_000_000;
  let row: RecentUsageRow = (Number.isFinite(numericId) ? state.logs.rowById.get(numericId) : undefined)
    || state.logs.rows.find((r) => r.request_id === requestId)
    || inflight
    || {
      id: numericId || 0, request_id: requestId, provider_id: "", upstream_model_id: "",
      created_at: new Date().toISOString(), status_code: 0, total_ms: 0,
      prompt_tokens: null, completion_tokens: null, cost_usd: 0,
      is_streaming: false, stream_complete: false, race_lost: false,
      trace_id: traceId || "", connect_ms: null, ttft_ms: null,
      request_body_json: null, response_body_json: null,
      request_headers: null, response_headers: null,
      race_total: null, race_attempts: null,
      error_message: null,
      stop_reason: null,
      compression_savings_pct: null,
      compression_techniques: null,
    };
  // In-flight / ghost entries have no DB row (id === 0 / synthetic id
  // / status_code === 0). Skip the /usage/detail fetch — it would
  // 404/500 with "not found in database" — and surface a clear reason
  // in the modal instead. This covers race-loser ghosts whose terminal
  // event arrived but whose usage row was dropped (DB lock timeout).
  const isInflight: boolean = !!inflight || (isSyntheticId && row.status_code === 0);
  // Do not merge inflight/ghost rows into `state.logs.rows` — they
  // have id 0 / synthetic ids and would duplicate the inflight
  // placeholder already rendered from the inflight maps.
  if (!isInflight && !state.logs.rows.find((r) => r.id === row.id)) {
    state.logs.rows = mergeLogsByDescId(state.logs.rows, [row]);
  }
  state.logs.selectedRow = row;

  if (isInflight) {
    const stage: StageEvent | undefined =
      (traceId ? state.logs.stagesByTraceId.get(traceId) : undefined) ||
      (requestId ? state.logs.stagesByRequestId.get(requestId) : undefined);
    const terminal: boolean =
      !!stage &&
      (stage.stage === "cancelled" || stage.stage === "failed" || stage.stage === "completed");
    if (!row.error_message) {
      if (terminal) {
        row.error_message = stage && stage.stage === "cancelled"
          ? "Cancelled (race loser) — no recorded detail."
          : "Request ended without a recorded detail row.";
      } else {
        // Improve the "still in progress" message: if the row is
        // older than 30s with no new stage events, surface a more
        // actionable message so the operator knows the stream is
        // likely stalled (not just slow). The proxy will record a
        // failure row after the idle-chunk timeout (default 120s).
        const ageMs: number = row.created_at
          ? Date.now() - new Date(row.created_at).getTime()
          : 0;
        const stageName: string = stage?.stage ?? "unknown";
        const elapsedMs: number | undefined = stage?.elapsed_ms;
        if (ageMs > 30_000) {
          const ageSec: number = Math.round(ageMs / 1000);
          row.error_message = `Stream appears stalled — last stage "${stageName}" ${ageSec}s ago. ` +
            `The proxy will record a failure row after the idle-chunk timeout (default 120s). ` +
            `Use "Copy debug bundle" to capture the current state for debugging.`;
        } else {
          row.error_message = `Request in progress — current stage: "${stageName}"` +
            (elapsedMs != null ? ` (${elapsedMs}ms elapsed)` : "") +
            `. Detail will be available when the stream completes.`;
        }
      }
    }
    showLogDetail(row as unknown as LogDetailLog);
    return;
  }

  // Fetch detail FIRST if the row is incomplete (broadcast rows have
  // request_body_json / response_body_json redacted). Then render
  // the modal with complete data — avoids the "flash of not recorded".
  if (!hasCompleteLogDetail(row as unknown as LogDetailLog) && Number.isFinite(numericId) && numericId > 0) {
    try {
      const detail = await api(`/usage/detail?id=${encodeURIComponent(id)}`) as { row?: RecentUsageRow; detail?: RecentUsageRow } | RecentUsageRow | null;
      const fetched = (detail && typeof detail === "object" && ("row" in detail || "detail" in detail))
        ? (detail.row ?? detail.detail ?? null)
        : (detail as RecentUsageRow | null);
      if (fetched) {
        const merged: Record<string, unknown> = { ...row } as unknown as Record<string, unknown>;
        for (const [k, v] of Object.entries(fetched as unknown as Record<string, unknown>)) {
          if (v != null) merged[k] = v;
        }
        row = merged as unknown as RecentUsageRow;
        state.logs.rowById.set(Number(row.id || id), row);
        state.logs.selectedRow = row;
      }
    } catch (e: unknown) {
      // Non-fatal: render with whatever we have — the modal will
      // show "not recorded" for missing fields, which is truthful.
      showToast(`Request detail unavailable: ${e instanceof Error ? e.message : String(e)}`, "error");
    }
  }
  // Render the modal with the best data we have (possibly enriched
  // by the detail call above).
  showLogDetail(row as unknown as LogDetailLog);
}
if (typeof window !== "undefined") {
  const w = window as unknown as { openLogDetail?: typeof openLogDetail };
  w.openLogDetail = openLogDetail;
}

// ---- Mount -------------------------------------------------------------

export async function mountLogs(): Promise<(() => void) | void> {
  const main = document.getElementById("main");
  if (!main) return;

  // Reset view-local UI state on every mount. The router calls the
  // previous view's cleanup (which stops the WS, ticker, and reaper)
  // before mounting this one, so we re-initialise everything here.
  columnsMenuOpen = false;

  // First-mount of this view: restore the user's column-visibility
  // choice from localStorage, falling back to "all visible". Done
  // here (not at module load) so it only runs in the browser, and
  // only when the user actually navigates to /logs.
  if (!state.logs.visibleColumns) {
    state.logs.visibleColumns = loadVisibleColumns();
  }
  state.logs.rows = [];
  state.logs.rowById = new Map();
  state.logs.lastSeenId = 0;
  state.logs.liveTokens = new Map();
  state.logs.reconnectAttempt = 0;
  state.logs.page = 1;
  state.logs.followTail = true;
  // Clear stale inflight/stage state from previous sessions so
  // old ghost entries (left by aborted race losers before the
  // grace-period fix) don't survive across view navigations.
  state.logs.inflightByTraceId = new Map();
  state.logs.inflightByRequestId = new Map();
  state.logs.stagesByTraceId = new Map();
  state.logs.stagesByRequestId = new Map();

  // Mount the lit-html view. `mountView` registers the render
  // function with the reactive system so `requestUpdate()` (called
  // from the WS handler, pagination handlers, etc.) triggers a
  // microtask-coalesced re-render.
  const cleanupReactive = mountView(main, renderLogsView);

  // Wire the document-level outside-click handler for the columns
  // menu. Bound once per session; the handler reads `columnsMenuOpen`
  // (module-local) so it stays in sync with the template. Clicks
  // inside the `.columns-menu-wrapper` are left alone so the user
  // can interact with the checkboxes without closing the menu.
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

  // Start the WS, ticker, and reaper. All three are idempotent
  // (they check their own handle before starting), but we always
  // call them here so a fresh mount after cleanup gets a fresh
  // connection / interval.
  fetchRecordingState();
  setMessageHandler(handleLogsMessage);
  connectLogsWebSocket();
  startLogLatencyTicker();
  startStaleInflightReaper();

  // Cleanup: tear down all three (WS, ticker, reaper) and release
  // the lit-html container so the next view's `mountView` doesn't
  // race with our `requestUpdate()`.
  return () => {
    disconnectLogsWebSocket();
    stopLogLatencyTicker();
    stopStaleInflightReaper();
    cleanupReactive();
  };
}
