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
import { repeat } from "lit-html/directives/repeat.js";
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
import { fetchRecordingState, toggleRecording } from "../components/recording-toggle.js";
import { showToast } from "../components/toast.js";
import { mountView, requestUpdate } from "../state/reactive.js";
import {
  showLogDetail,
  updateOpenLogDetail,
  hasCompleteLogDetail,
  bumpOpenLogDetailGeneration,
  isCurrentOpenLogDetailGeneration,
} from "../components/log-detail.js";
import type { RecentUsageRow, StageEvent } from "../lib/types/api.js";
import type { NotificationEvent } from "../lib/types/notifications.js";

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

// WebSocket message envelope. The server sends one of several payload
// types — see `handleLogsMessage` below for the per-branch handling. We
// model the discriminated union on `type` and let the per-branch data be
// a tight union of the known payload shapes (consumers type-guard before
// reading). `export`d so `state/ws-bus.ts` (F2) can subscribe by type.
//
// F2 added `'notification'` to the `type` union and `NotificationEvent`
// to the `data` union — the dashboard's notifications tray subscribes to
// the new type via `subscribeWs('notification', ...)` in `state/ws-bus.ts`.
//
// We mark fields optional so old code that accesses `msg.data ?? msg.row
// ?? msg` keeps type-checking; consumers narrow via type-guards. The
// `channel` field is set on `lag_warning` envelopes so the client can
// distinguish `notifications` lags from `usage` / `stage` lags and refetch
// from the appropriate REST endpoint (notifications are refetched via
// `GET /admin/api/notifications`; usage/stage use the `resync` envelope).
export interface WsEnvelope {
  type:
    | "history"
    | "row"
    | "stage"
    | "lag_warning"
    | "resync"
    | "pong"
    | "error"
    | "notification";
  data?: StageEvent | RecentUsageRow | NotificationEvent;
  row?: unknown;
  rows?: RecentUsageRow[];
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
  // F2: `channel` is set on `lag_warning` envelopes to tell the
  // client which broadcast channel lagged. `notifications` lags
  // should refetch via `GET /admin/api/notifications`; `usage` /
  // `stage` lags are followed by a `resync` envelope with
  // `since_id`. Absent on `resync` envelopes.
  channel?: "usage" | "stage" | "notifications";
  since_id?: number;
  // F2: `server_time` is set on `pong` envelopes (the server's
  // RFC-3339 timestamp when it processed the client's `ping`).
  // Already in use by the existing pong handler.
  server_time?: string;
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
  // Key strategy: each attempt is a SEPARATE row, keyed by its unique
  // `trace_id` (per-attempt). Retries of the same `request_id` have
  // DIFFERENT `trace_id`s and MUST NOT be collapsed into one row —
  // collapsing causes the "row flickers / overlaps" bug where a new
  // attempt's data overlays the previous attempt's data.
  //
  // Identity precedence:
  //   1. DB id (for finalized rows with id > 0) — `id:<id>`
  //   2. trace_id (for inflight rows) — `tid:<trace_id>`
  //   3. request_id (only when trace_id is empty) — `rid:<request_id>`
  //
  // We do NOT use a `req:<request_id>` secondary alias anymore. The
  // old alias caused race attempts (same request_id, different
  // trace_id) to collapse into one row, which:
  //   - made the table flicker as each attempt's data overlaid the last
  //   - showed the WRONG attempt's model/tokens/latency
  //   - broke the "new requests don't appear until they finish" flow
  //     (the inflight placeholder was merged into a finalized row)
  const merged = new Map<string, RecentUsageRow>();
  const rowKey = (r: RecentUsageRow): string => {
    if (r.id && r.id > 0) return `id:${r.id}`;
    if (r.trace_id) return `tid:${r.trace_id}`;
    if (r.request_id) return `rid:${r.request_id}`;
    return `fallback:${Math.random()}`;
  };
  for (const row of existing) {
    merged.set(rowKey(row), row);
  }
  for (const row of incoming) {
    if (row == null) continue;
    const k = rowKey(row);
    let base = merged.get(k);
    // Inflight rows (id=0) with a trace_id: match by trace_id only.
    // Do NOT match by request_id — sibling race attempts share the
    // same request_id and must stay separate.
    if ((!row.id || row.id === 0) && row.trace_id) {
      const tidKey = `tid:${row.trace_id}`;
      if (merged.has(tidKey)) base = merged.get(tidKey) as RecentUsageRow;
    }
    // Merge: start from base, then overlay incoming fields. BUT —
    // only overlay fields that are not null/undefined in the incoming
    // row. This prevents a re-broadcast (e.g. the client_response
    // UPDATE re-broadcast) from clobbering fields it doesn't carry
    // (cost_usd, compression_savings_pct, etc.) with null.
    const mergedRow: RecentUsageRow = { ...(base || {} as RecentUsageRow) };
    const target = mergedRow as unknown as Record<string, unknown>;
    for (const [key, value] of Object.entries(row)) {
      if (value !== null && value !== undefined) {
        target[key] = value;
      }
    }
    merged.set(k, mergedRow);
    if (row.id && row.id > 0) state.logs.lastSeenId = Math.max(state.logs.lastSeenId, row.id);
  }
  // Build the result array from primary-keyed entries only (no aliases).
  const result: RecentUsageRow[] = Array.from(merged.values());
  result.sort((a, b) => {
    // Finalized rows (id > 0) sort by id descending. Inflight rows
    // (id === 0) sort by created_at descending so the newest
    // inflight appears at the top.
    if (a.id && a.id > 0 && b.id && b.id > 0) return b.id - a.id;
    const aTime = Date.parse(a.created_at || "") || 0;
    const bTime = Date.parse(b.created_at || "") || 0;
    return bTime - aTime;
  });
  const limit = state.logs.maxRows;
  if (result.length > limit) {
    const finalResult = result.slice(0, limit);
    // Rebuild the merged map without the trimmed entries so
    // rowById lookups don't return stale rows.
    const trimmed = new Map<string, RecentUsageRow>();
    for (const r of finalResult) {
      trimmed.set(rowKey(r), r);
    }
    state.logs.rowById = trimmed as unknown as Map<number, RecentUsageRow>;
    return finalResult;
  }
  state.logs.rowById = merged as unknown as Map<number, RecentUsageRow>;
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
      <div class="logs-scroll-area" id="logs-scroll-area">
        ${renderHeaderRow(visibleColKeys)}
        ${pageRows.length === 0
          ? html`<div class="empty" style="padding:2rem;">No recent requests yet. Use the API to see logs appear here in real time.</div>`
          : repeat(
              pageRows,
              (r: RecentUsageRow) => r.trace_id || `id:${r.id}` || `req:${r.request_id}`,
              (r: RecentUsageRow) => html`<div data-key=${r.trace_id || `id:${r.id}` || `req:${r.request_id}`}>${renderLogRow(r, visibleColKeys)}</div>`,
            )}
      </div>
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
    // Rendering handled by the 250ms render interval in mountLogs — no requestUpdate() here.
    if (state.currentView?.name === "logs") updateOpenLogDetail(exactRow as unknown as LogDetailLog);
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
      client_response: false,
      prompt_tokens_estimated: false,
      completion_tokens_estimated: false,
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
      client_response: false,
      prompt_tokens_estimated: false,
      completion_tokens_estimated: false,
    });
  } else if (!traceId && state.logs.inflightByRequestId.has(requestId)) {
    const existing = state.logs.inflightByRequestId.get(requestId)!;
    if (event.upstream_model_id) existing.upstream_model_id = event.upstream_model_id;
    if (event.provider_id) existing.provider_id = event.provider_id;
    if (event.stage === "streaming") existing.is_streaming = true;
    if (event.status_code > 0) existing.status_code = event.status_code;
  }
  if (state.logs.followTail) state.logs.page = 1;
  // Rendering handled by the 250ms render interval in mountLogs — no requestUpdate() here.
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
    }
  };
  scan(state.logs.inflightByTraceId, true);
  scan(state.logs.inflightByRequestId, false);
  // Only re-render when something actually changed — avoids a full
  // table re-render every tick while ordinary inflight entries exist.
  // Rendering handled by the 250ms render interval in mountLogs.
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
      // Rendering handled by the 250ms render interval in mountLogs — no requestUpdate() here.
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
  // CRASH FIX: only call requestUpdate() if the logs view is the
  // currently mounted view. During boot, the WS opens (via
  // initNotificationsStore in the sidebar) BEFORE the router has
  // mounted any view into #main. If a WS message arrives during
  // this window and we call requestUpdate(), lit-html tries to
  // render the logs template (which uses `repeat`) into a container
  // that either doesn't exist yet or belongs to a different view —
  // causing "can't access property 'data', this._$AA.nextSibling
  // is null" (a lit-html internal crash).
  //
  // We still update `state.logs.*` (so the data is fresh when the
  // user navigates to logs), but we skip the requestUpdate() call.
  const isLogsViewActive: boolean = state.currentView?.name === "logs";
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
    state.logs.page = 1; state.logs.followTail = true;
    // Rendering handled by the 250ms render interval in mountLogs.
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
    // Rendering handled by the 250ms render interval in mountLogs.
    if (isLogsViewActive) updateOpenLogDetail(row as unknown as LogDetailLog);
  } else if (msg.type === "stage") {
    const candidate = msg.data ?? msg;
    if (isStageEventShape(candidate)) {
      handleStageEvent(candidate);
    }
  } else if (msg.type === "error") {
    showToast(msg.message || "Live Logs WebSocket error", "error");
  } else if (msg.type === "lag_warning") {
    // H7 fix: the server detected a broadcast `Lagged(_)` on
    // either the row, stage, or notifications channel. A `resync`
    // envelope follows immediately for row/stage lags (handled
    // below). Show a persistent banner so the operator knows the
    // displayed log is not a complete picture; the resync fetch
    // will fill in the gap in the background.
    //
    // F2: a `notifications` lag does NOT carry a `resync` follow-up
    // — the notifications tray (F4) refetches via
    // `GET /admin/api/notifications` (notifications are persisted,
    // so the REST list is the source of truth). We show a different
    // toast so the operator knows it's the tray that lagged, not
    // the live-logs feed.
    const skipped = Number(msg.skipped || 0);
    if (msg.channel === "notifications") {
      showToast(
        `Notifications feed lagged; ${skipped} event(s) skipped. ` +
          `Refetching the tray…`,
        "warning",
      );
    } else {
      showToast(
        `Live Logs broadcast lagged; ${skipped} event(s) skipped. ` +
          `Refetching to catch up…`,
        "warning",
      );
    }
  } else if (msg.type === "resync") {
    // H7 fix: the server lost us on a broadcast channel and
    // is asking the dashboard to fetch any rows newer than
    // `since_id` to recover. This is the only path that
    // prevents permanent state loss for slow dashboards.
    const sinceId = Number(msg.since_id || 0);
    void resyncUsageRows(sinceId);
  } else if (msg.type === "notification") {
    // F2: notifications (model_new / model_gone / model_auto_activated /
    // system) are surfaced to the dashboard tray, NOT to the logs view.
    // The logs handler intentionally does nothing here — the envelope
    // is dispatched to ws-bus subscribers (notifications tray F4, live-
    // store F5) by `state/ws.ts` after this handler returns. Listed
    // explicitly so the discriminated union on `msg.type` is exhaustive
    // and a future `else if (msg.type === ...)` doesn't accidentally
    // swallow notifications.
  }
}

async function openLogDetail(id: string, requestId: string, traceId?: string): Promise<void> {
  // RACE-CONDITION GUARD: bump the generation counter at the very start.
  // Each `openLogDetail` call captures the current generation; after the
  // async `/usage/detail` fetch completes, we check whether this call is
  // still the most recent. If the user clicked another row in the
  // meantime (which bumps the generation again), the stale fetch's result
  // is discarded — it doesn't overwrite the modal the user is now looking
  // at. This prevents the "I clicked row A then row B, but the modal
  // flipped back to A when A's slower fetch finished" race.
  const gen: number = bumpOpenLogDetailGeneration();
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
  // CRITICAL: find the row by BOTH request_id AND trace_id (when trace_id
  // is available). The previous code used `state.logs.rows.find((r) =>
  // r.request_id === requestId)` which returns the FIRST row with a
  // matching request_id — in a combo with retries, multiple rows share
  // the same request_id but have different trace_ids (and potentially
  // different upstream_model_ids). The `.find()` returned the WRONG
  // attempt, causing the modal to show a different model's data than the
  // row the user clicked. This was the root cause of the "model name
  // changes while I'm debugging" bug.
  //
  // Also try `rowById.get(numericId)` first (for finalized rows with real
  // DB ids). Note: `rowById` is rebuilt by `mergeLogsByDescId` and may
  // have inconsistent key types (number vs string), so the lookup may
  // fail — the `rows.find()` fallback below handles this.
  let row: RecentUsageRow = (Number.isFinite(numericId) ? state.logs.rowById.get(numericId) : undefined)
    || (traceId
      ? state.logs.rows.find((r) => r.request_id === requestId && r.trace_id === traceId)
      : state.logs.rows.find((r) => r.request_id === requestId))
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
      client_response: false,
      prompt_tokens_estimated: false,
      completion_tokens_estimated: false,
    };
  // In-flight / ghost entries have no DB row (id === 0 / synthetic id
  // / status_code === 0). Skip the /usage/detail fetch — it would
  // 404/500 with "not found in database" — and surface a clear reason
  // in the modal instead. This covers race-loser ghosts whose terminal
  // event arrived but whose usage row was dropped (DB lock timeout).
  // ALSO: any row with a synthetic ID (Number.MAX_SAFE_INTEGER - ts,
  // which is > 1_000_000_000) is an inflight/ghost regardless of
  // status_code — the real DB row hasn't arrived yet. Without this
  // check, clicking a completed-but-still-inflight row sends the
  // synthetic ID to /usage/detail and gets a 500.
  const isInflight: boolean = !!inflight || isSyntheticId || (row.status_code === 0);
  // Do not merge inflight/ghost rows into `state.logs.rows` — they
  // have id 0 / synthetic ids and would duplicate the inflight
  // placeholder already rendered from the inflight maps.
  if (!isInflight && !state.logs.rows.find((r) => r.id === row.id)) {
    state.logs.rows = mergeLogsByDescId(state.logs.rows, [row]);
  }
  // SNAPSHOT: do NOT store a live reference to the row object. Create a
  // shallow copy so mutations to the original (e.g. inflight placeholders
  // being updated by stage events, or `mergeLogsByDescId` replacing the
  // row in `state.logs.rows`) don't affect the modal's data. The modal
  // reads from `state.logs.selectedRow` via `copyDebugBundle` and
  // `updateOpenLogDetail`; if it's a live reference, the modal's data
  // silently changes as background requests mutate the original object.
  // This is the core decoupling fix.
  state.logs.selectedRow = { ...row } as typeof state.logs.selectedRow;

  if (isInflight) {
    // SNAPSHOT: work on a copy so we don't mutate the LIVE inflight
    // placeholder (which is rendered in the table and may be updated by
    // future stage events). The synthesized error_message below is for
    // the MODAL only — it should not leak back into the table row.
    row = { ...row };
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
  // Skip the fetch for synthetic IDs (inflight/ghost entries) — they
  // don't exist in the DB and the fetch would 500.
  if (!hasCompleteLogDetail(row as unknown as LogDetailLog)
      && Number.isFinite(numericId)
      && numericId > 0
      && !isSyntheticId) {
    try {
      const detail = await api(`/usage/detail?id=${encodeURIComponent(id)}`) as { row?: RecentUsageRow; detail?: RecentUsageRow } | RecentUsageRow | null;
      // RACE-CONDITION GUARD: if the user clicked another row while
      // this fetch was in flight, `gen` is no longer current. Discard
      // the result — the user is now looking at a different row's
      // modal, and applying this fetch's data would either overwrite
      // the wrong modal or open a stale modal on top of the current
      // one. This is the second half of the generation-counter fix
      // (the first half is `bumpOpenLogDetailGeneration` at the top).
      if (!isCurrentOpenLogDetailGeneration(gen)) return;
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
        // SNAPSHOT: store a shallow copy, not a live reference. `row`
        // here is already a new object (from the spread above), but we
        // snapshot it anyway for consistency — the contract is that
        // `selectedRow` is ALWAYS a snapshot, never a live reference.
        state.logs.selectedRow = { ...row } as typeof state.logs.selectedRow;
      }
    } catch (e: unknown) {
      // RACE-CONDITION GUARD: same check on the error path — if the
      // user navigated away, don't show a stale toast.
      if (!isCurrentOpenLogDetailGeneration(gen)) return;
      // Non-fatal: render with whatever we have — the modal will
      // show "not recorded" for missing fields, which is truthful.
      showToast(`Request detail unavailable: ${e instanceof Error ? e.message : String(e)}`, "error");
    }
  }
  // RACE-CONDITION GUARD: final check before opening the modal. If the
  // user clicked another row during the fetch (or during the inflight
  // branch above), don't open this modal — the user's latest click
  // should win.
  if (!isCurrentOpenLogDetailGeneration(gen)) return;
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

  fetchRecordingState();
  // CRITICAL: register the message handler IMMEDIATELY so stage
  // events arriving via WS are processed into inflight placeholders
  // even before the first render completes. The 250ms render interval
  // picks up the inflight data on the next tick.
  // The handler is also registered at module load (see bottom of file)
  // so stage events arriving during boot (before mountLogs runs) are
  // captured.
  setMessageHandler(handleLogsMessage);
  connectLogsWebSocket();
  // CRASH FIX: do NOT start the latency ticker. The ticker (100ms
  // interval) modifies DOM nodes directly (sub.textContent = ...)
  // which conflicts with lit-html's diffing. When the 250ms render
  // interval re-renders the template, lit-html finds DOM nodes in a
  // state that doesn't match its internal template tree → crash
  // 'nextSibling is null'. The live latency is now computed in the
  // render function itself (renderLogRow reads stage.timestamp and
  // computes elapsed = Date.now() - timestamp on each render). The
  // 250ms render interval refreshes it at 4Hz — slightly less smooth
  // than the ticker's 10Hz but without the DOM conflict.
  // startLogLatencyTicker();  // DISABLED — see comment above
  startStaleInflightReaper();

  // CRASH FIX: render on a fixed 250ms interval instead of calling
  // requestUpdate() from WS message handlers. The previous approach
  // (requestUpdate on every WS message) crashed lit-html because:
  // 1. WS messages arrive asynchronously during boot, before the
  //    view's DOM is fully initialized
  // 2. The ticker modifies DOM nodes directly (textContent), which
  //    can corrupt lit-html's internal state when requestUpdate()
  //    tries to diff against the modified DOM
  // 3. Rapid requestUpdate() calls (multiple per second under load)
  //    overwhelm lit-html's microtask coalescing
  //
  // The interval approach decouples data updates (WS handlers modify
  // state.logs.*) from rendering (the interval calls requestUpdate()
  // at a controlled 4Hz cadence). This is the same pattern the
  // live-store uses for the home dashboard.
  let renderInterval: ReturnType<typeof setInterval> | null = setInterval(() => {
    requestUpdate();
  }, 250);

  return () => {
    if (renderInterval) {
      clearInterval(renderInterval);
      renderInterval = null;
    }
    // Do NOT clear the message handler on unmount. The handler only
    // updates state.logs.* (no requestUpdate, no DOM mutation) so it's
    // safe to keep running. This ensures stage events arriving while
    // the user is on another view are still processed — inflight
    // placeholders are created so when the user navigates back to logs,
    // they see the live requests immediately. The 250ms render interval
    // (which only runs while logs is mounted) picks up the data.
    // setMessageHandler(null);  // intentionally NOT called
    disconnectLogsWebSocket();
    // stopLogLatencyTicker();  // not started — see comment in mount body
    stopStaleInflightReaper();
    cleanupReactive();
  };
}

// Register the WS message handler at module load time so stage events
// arriving during boot (before the user navigates to the logs view) are
// captured into inflight placeholders. The handler is safe to call at
// any time — it only updates state.logs.* data, never touches the DOM
// (rendering is done by the 250ms render interval in mountLogs).
setMessageHandler(handleLogsMessage);
