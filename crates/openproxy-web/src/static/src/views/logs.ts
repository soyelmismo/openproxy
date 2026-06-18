// views/logs.ts — live logs view. The biggest view by far;
// historical rows + in-flight placeholders, paginated, with a
// 100ms latency ticker and a recording toggle. The detail modal
// lives in components/log-detail.ts to keep this file under the
// 400-LOC cap.

import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { cssEscape } from "../lib/css-escape.js";
import { escapeHtml } from "../lib/escape.js";
import { renderLogRowHtml } from "../components/log-row.js";
import { LOG_COLUMNS, LOGS_VISIBLE_COLUMNS_STORAGE_KEY } from "../lib/constants.js";
import { connectLogsWebSocket, setMessageHandler } from "../state/ws.js";
import { startLogLatencyTicker } from "../state/ticker.js";
import { fetchRecordingState, toggleRecording } from "../components/recording-toggle.js";
import { showToast } from "../components/toast.js";
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
}

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

function attachLogRowHandlers(): void {
  document.querySelectorAll("#logs .log-row").forEach((row) => {
    row.addEventListener("click", () => {
      const el = row as HTMLElement;
      // `dataset` is `{ [name: string]: string | undefined }`
      // (an index signature), so under `noPropertyAccessFromIndexSignature`
      // we have to use bracket access.
      const id = el.dataset["id"] || "";
      const requestId = el.dataset["requestId"] || "";
      openLogDetail(id, requestId);
    });
  });
}

function renderLogsRows(): void {
  const logsEl = document.getElementById("logs");
  if (!logsEl) return;
  const inflightRows: (RecentUsageRow & { __inflight: boolean })[] = [
    ...Array.from(state.logs.inflightByTraceId.values()),
    ...Array.from(state.logs.inflightByRequestId.values()),
  ].map((p) => {
    const t = Date.parse(p.created_at);
    const syntheticId = isFinite(t) ? (Number.MAX_SAFE_INTEGER - t) : Number.MAX_SAFE_INTEGER;
    return Object.assign({}, p, { id: syntheticId, __inflight: true });
  });
  const rows = (state.logs.rows as (RecentUsageRow & { __inflight?: boolean })[]).concat(inflightRows).sort((a, b) => (b.id || 0) - (a.id || 0));
  const totalRows = rows.length;
  const rpp = state.logs.rowsPerPage;
  const totalP = Math.max(1, Math.ceil(totalRows / rpp));
  if (state.logs.page > totalP) state.logs.page = totalP;
  if (state.logs.page < 1) state.logs.page = 1;
  const start = (state.logs.page - 1) * rpp;
  const end = Math.min(start + rpp, totalRows);
  const pageRows = rows.slice(start, end);
  // Build the header row from LOG_COLUMNS so the set of columns
  // and the header text are always in sync. Each <span> gets the
  // .log-{key} class so styling matches the body cells.
  const visibleColKeys = state.logs.visibleColumns || new Set(LOG_COLUMNS.map((c) => c.key));
  const headerCells = LOG_COLUMNS
    .filter((c) => visibleColKeys.has(c.key))
    .map((c) => `<span class="log-${c.key}" data-col="${c.key}">${escapeHtml(c.label)}</span>`)
    .join("");
  const headerHtml = `
    <div class="log-row" style="cursor:default;border-bottom:1px solid var(--color-border);font-weight:600;font-size:0.72rem;text-transform:uppercase;color:var(--color-text-muted);background:var(--color-log-header-bg);position:sticky;top:0;z-index:1;">
      ${headerCells}
    </div>`;
  const bodyHtml = pageRows.length
    ? pageRows
        .map((r) => {
          // Resolve the live stage for this row. Primary key is
          // `trace_id` so each attempt of a multi-attempt request
          // (per-target retry, fallback to next combo target, race
          // loser) has its own phase. The request_id fallback is
          // only for the edge case where a row's `trace_id` is
          // empty (synthetic events emitted from the frontend
          // itself).
          const stage: StageEvent | undefined =
            (r.trace_id && state.logs.stagesByTraceId.get(r.trace_id)) ||
            (r.request_id && state.logs.stagesByRequestId.get(r.request_id)) ||
            undefined;
          return renderLogRowHtml(r, stage, visibleColKeys);
        })
        .join("")
    : '<div class="empty" style="padding:2rem;">No recent requests yet. Use the API to see logs appear here in real time.</div>';
  const isFirst = state.logs.page <= 1;
  const isLast = state.logs.page >= totalP;
  const paginationHtml = totalRows > 0 ? `
    <div class="logs-pagination">
      <span class="rows-info">${totalRows} row${totalRows !== 1 ? "s" : ""}</span>
      <button data-action="logsGoPage" data-arg1="1" ${isFirst ? "disabled" : ""}>⟨⟨</button>
      <button data-action="logsPrevPage" ${isFirst ? "disabled" : ""}>‹ Prev</button>
      <span class="page-info">Page ${state.logs.page} of ${totalP}</span>
      <button data-action="logsNextPage" ${isLast ? "disabled" : ""}>Next ›</button>
      <button data-action="logsGoPage" data-arg1="${totalP}" ${isLast ? "disabled" : ""}>⟩⟩</button>
      <label class="logs-follow-toggle" title="When ON, new rows automatically scroll the view to the most recent page. When OFF, the view stays on the page you are reading.">
        <input type="checkbox" id="logs-follow-input" ${state.logs.followTail ? "checked" : ""} data-action="logsSetFollow">
        <span>Follow</span>
      </label>
    </div>` : "";
  logsEl.innerHTML = headerHtml + bodyHtml + paginationHtml;
  attachLogRowHandlers();
}

export function logsPrevPage(): void {
  if (state.logs.page > 1) {
    state.logs.page--;
    if (state.logs.page === 1) state.logs.followTail = true;
    renderLogsRows();
  }
}
export function logsNextPage(): void {
  if (state.logs.page < totalPages()) {
    state.logs.page++;
    if (state.logs.page >= totalPages()) state.logs.followTail = false;
    renderLogsRows();
  }
}
export function logsGoPage(p: number): void {
  const total = totalPages();
  state.logs.page = Math.max(1, Math.min(p, total));
  state.logs.followTail = (state.logs.page === 1);
  renderLogsRows();
}
export function logsSetFollow(e: Event): void {
  const target = e.target;
  let enabled = false;
  if (target instanceof HTMLInputElement) {
    enabled = !!target.checked;
  }
  state.logs.followTail = enabled;
  if (enabled) { state.logs.page = 1; renderLogsRows(); }
}

// ---- Columns menu ------------------------------------------------------
// Render the popover menu body (one checkbox per column). Called
// both at mount time and after each toggle so the checked state
// stays in sync with state.logs.visibleColumns.
function renderColumnsMenuBody(menuEl: HTMLElement | null): void {
  if (!menuEl) return;
  const visible = state.logs.visibleColumns || new Set(LOG_COLUMNS.map((c) => c.key));
  menuEl.innerHTML = LOG_COLUMNS.map((c) => {
    const checked = visible.has(c.key) ? "checked" : "";
    // data-action="toggleColumn" data-arg1="<key>" — the global
    // click shim in app.js dispatches this to HANDLERS.toggleColumn.
    return `<label class="columns-menu-item"><input type="checkbox" data-action="toggleColumn" data-arg1="${c.key}" ${checked}><span>${escapeHtml(c.label)}</span></label>`;
  }).join("");
}

export function toggleColumnsMenu(): void {
  const menu = document.querySelector(".columns-menu");
  if (menu) {
    const isOpen = menu.classList.toggle("open");
    // Keep aria-expanded in sync so screen readers announce the
    // menu's state, and so the close-on-outside-click handler can
    // inspect it cheaply.
    const btn = document.getElementById("logs-columns-toggle");
    if (btn) btn.setAttribute("aria-expanded", isOpen ? "true" : "false");
  }
}

export function toggleColumn(key: string): void {
  if (!state.logs.visibleColumns) {
    state.logs.visibleColumns = new Set(LOG_COLUMNS.map((c) => c.key));
  }
  const set = state.logs.visibleColumns;
  if (set.has(key)) {
    // Refuse to hide the last visible column — an empty table is
    // useless and the user has no "show all" affordance without
    // toggling each one back on. We don't mutate the set, but
    // the browser's default click has already flipped the
    // checkbox to unchecked — we flip it back below to keep the
    // DOM in sync with state.
    if (set.size === 1) {
      const cb = document.querySelector(`.columns-menu input[data-arg1="${key}"]`) as HTMLInputElement | null;
      if (cb) cb.checked = true;
      return;
    }
    set.delete(key);
  } else {
    set.add(key);
  }
  saveVisibleColumns();
  // Re-render the table so the header and body both reflect the
  // new visibility. We do NOT re-render the menu's innerHTML
  // here — the user is mid-click on a checkbox and that same
  // click event is still bubbling up to the document-level
  // close-on-outside-click handler. Replacing innerHTML detaches
  // `event.target`, which would make the wrapper-contains check
  // return false and close the menu. Update the checkbox's
  // `checked` attribute in place instead.
  const cb = document.querySelector(`.columns-menu input[data-arg1="${key}"]`) as HTMLInputElement | null;
  if (cb) cb.checked = set.has(key);
  renderLogsRows();
}

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
    renderLogsRows();
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
    });
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
    });
  }
  if (state.logs.followTail) state.logs.page = 1;
  renderLogsRows();
}

// Mirror the stage event into the two stage maps keyed by
// `trace_id` (primary) and `request_id` (fallback for events
// with an empty `trace_id`). Centralized so callers can't forget
// to update one of the two.
function setStage(event: StageEvent, requestId: string): void {
  const traceId = event.trace_id || "";
  if (traceId) {
    state.logs.stagesByTraceId.set(traceId, event);
  } else {
    state.logs.stagesByRequestId.set(requestId, event);
  }
}

function handleStreamTokens(msg: WsEnvelope): void {
  const requestId = msg.request_id;
  if (!requestId) return;
  // `liveTokens` is typed as `Map<LogsRequestId, number>` in the
  // state, but the live code stores strings (it's a running
  // concatenation of SSE deltas). The state type is a pre-existing
  // mis-shape; the runtime works with a string here. We cast
  // through unknown to keep the type checker happy without
  // rewriting the state contract.
  const tokensMap = state.logs.liveTokens as unknown as Map<string, string>;
  const prev = tokensMap.get(requestId) || "";
  const next = prev + (msg.delta || "");
  tokensMap.set(requestId, next);
  if (msg.complete) {
    const row = state.logs.rowById.get(msg.id ?? -1) || state.logs.rows.find((r) => r.request_id === requestId);
    if (row) { row.stream_complete = true; renderLogsRows(); }
  }
  const panel = document.querySelector('[data-token-panel="' + cssEscape(requestId) + '"]');
  const body = document.getElementById("stream-token-body");
  if (panel) panel.textContent = next;
  if (body) { body.textContent = next; body.scrollTop = body.scrollHeight; }
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
    // the DB, not a live stream). Mark each one as completed/failed
    // in the stage map so the latency ticker doesn't keep ticking
    // on them when the user scrolls the page.
    for (const r of rows) {
      if (!r || !r.request_id) continue;
      if ((!r.is_streaming || r.stream_complete) && r.status_code > 0) {
        // Index by `trace_id` so each attempt of a multi-attempt
        // request (per-target retry, fallback) keeps its own
        // phase. With this, the historical row's phase is locked
        // to its own attempt and is not overwritten when a later
        // attempt of the same `request_id` arrives.
        const synth: StageEvent = {
          request_id: r.request_id,
          stage: r.status_code >= 400 ? "failed" : "completed",
          elapsed_ms: r.total_ms || 0,
          status_code: r.status_code,
          timestamp: r.created_at || new Date().toISOString(),
          trace_id: r.trace_id,
          provider_id: r.provider_id,
          upstream_model_id: r.upstream_model_id,
          connect_ms: r.connect_ms,
          ttft_ms: r.ttft_ms,
          error: r.error_message ?? null,
        };
        if (r.trace_id) {
          state.logs.stagesByTraceId.set(r.trace_id, synth);
        } else {
          state.logs.stagesByRequestId.set(r.request_id, synth);
        }
      }
    }
    state.logs.page = 1; state.logs.followTail = true; renderLogsRows();
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
    // A request is considered "finished" when one of these is true:
    //   * a non-streaming row arrived (status_code > 0 and not
    //     flagged as still streaming)
    //   * a streaming row arrived that has stream_complete = true
    //   * the row has a non-zero status_code with no streaming flags
    // (i.e. the response is no longer in flight).
    //
    // Until the row arrives, the only signal we have is the latest
    // `stage` event (which is "streaming" while chunks are coming
    // in and "failed" on a hard error). The backend doesn't emit a
    // "completed" stage event on the success path — the row IS the
    // completion signal — so we synthesize one here. Without this,
    // `state.logs.stagesByRequestId` keeps the last "streaming"
    // entry forever, and the latency ticker (state/ticker.ts) keeps
    // recomputing `Date.now() - stage.timestamp`, which grows
    // without bound, so the latency cell shows a number that climbs
    // indefinitely and the phase cell stays on "streaming".
    if (row.request_id) {
      const isFinished = (
        (!row.is_streaming || row.stream_complete) ||
        (row.status_code > 0 && !row.is_streaming) ||
        (row.status_code > 0 && row.stream_complete)
      );
      if (isFinished) {
        // Index by `trace_id` (the row's own trace_id, not the
        // request_id) so the row's phase does not get clobbered
        // by a later attempt of the same `request_id`. The
        // previous request_id-keyed write here is what caused
        // the user-reported "failed rows get their phase
        // rewritten to 'Started' when a retry comes in" bug.
        const synth: StageEvent = {
          request_id: row.request_id,
          stage: row.status_code >= 400 ? "failed" : "completed",
          // elapsed_ms is informational here; the ticker uses
          // timestamp to compute live, but it short-circuits on
          // stage === "completed" / "failed" so this value isn't
          // read for finished requests. Keep it consistent with
          // the row's total_ms for any future caller.
          elapsed_ms: row.total_ms || 0,
          status_code: row.status_code,
          timestamp: row.created_at || new Date().toISOString(),
          trace_id: row.trace_id,
          provider_id: row.provider_id,
          upstream_model_id: row.upstream_model_id,
          connect_ms: row.connect_ms,
          ttft_ms: row.ttft_ms,
          error: row.error_message ?? null,
        };
        if (row.trace_id) {
          state.logs.stagesByTraceId.set(row.trace_id, synth);
        } else {
          state.logs.stagesByRequestId.set(row.request_id, synth);
        }
      }
    }
    if (state.logs.followTail) state.logs.page = 1;
    renderLogsRows();
    updateOpenLogDetail(row as unknown as LogDetailLog);
  } else if (msg.type === "stage") {
    const candidate = msg.data ?? msg;
    if (isStageEventShape(candidate)) {
      handleStageEvent(candidate);
    }
  } else if (msg.type === "stream_tokens") {
    handleStreamTokens(msg);
  } else if (msg.type === "error") {
    showToast(msg.message || "Live Logs WebSocket error", "error");
  }
}

async function openLogDetail(id: string, requestId: string): Promise<void> {
  const numericId = Number(id);
  let row: RecentUsageRow = (Number.isFinite(numericId) ? state.logs.rowById.get(numericId) : undefined)
    || state.logs.rows.find((r) => r.request_id === requestId)
    || {
      id: numericId || 0, request_id: requestId, provider_id: "", upstream_model_id: "",
      created_at: new Date().toISOString(), status_code: 0, total_ms: 0,
      prompt_tokens: null, completion_tokens: null, cost_usd: 0,
      is_streaming: false, stream_complete: false, race_lost: false,
      trace_id: "", connect_ms: null, ttft_ms: null,
      request_body_json: null, response_body_json: null,
      request_headers: null, response_headers: null,
      race_total: null, race_attempts: null,
      error_message: null,
    };
  if (!state.logs.rows.find((r) => r.id === row.id)) {
    state.logs.rows = mergeLogsByDescId(state.logs.rows, [row]);
  }
  state.logs.selectedRow = row;
  showLogDetail(row as unknown as LogDetailLog);
  if (!hasCompleteLogDetail(row as unknown as LogDetailLog)) {
    const detailEl = document.getElementById("log-detail-loading");
    if (detailEl) detailEl.textContent = "Loading detail…";
    try {
      const detail = await api(`/usage/detail?id=${encodeURIComponent(id)}`) as { row?: RecentUsageRow; detail?: RecentUsageRow } | RecentUsageRow | null;
      const fetched = (detail && typeof detail === "object" && ("row" in detail || "detail" in detail))
        ? (detail.row ?? detail.detail ?? null)
        : (detail as RecentUsageRow | null);
      if (fetched) {
        row = { ...row, ...fetched };
        state.logs.rowById.set(Number(row.id || id), row);
        state.logs.selectedRow = row;
        // Re-render the open modal in place with the freshly fetched
        // detail data. updateOpenLogDetail handles the DOM swap
        // (it no-ops if the modal is closed, but in openLogDetail
        // we know the modal is open because we called showLogDetail
        // a few lines above).
        updateOpenLogDetail(row as unknown as LogDetailLog);
      }
    } catch (e: unknown) {
      const err = e instanceof Error ? e : null;
      const msg = err ? err.message : String(e);
      if (detailEl) detailEl.textContent = `Detail unavailable: ${msg}`;
      showToast(`Request detail unavailable: ${msg}`, "error");
    }
  }
}
if (typeof window !== "undefined") {
  const w = window as unknown as { openLogDetail?: typeof openLogDetail };
  w.openLogDetail = openLogDetail;
}

export async function mountLogs(): Promise<void> {
  const main = document.getElementById("main");
  const alreadyRendered = main && main.querySelector(".logs-header") && main.querySelector("#logs");
  if (alreadyRendered) {
    setMessageHandler(handleLogsMessage);
    connectLogsWebSocket();
    fetchRecordingState();
    startLogLatencyTicker();
    return;
  }
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
  if (!main) return;
  main.innerHTML = `
    <div class="logs-header">
      <h2>Live Logs</h2>
      <div class="logs-header-actions">
        <div class="columns-menu-wrapper">
          <button id="logs-columns-toggle" data-action="toggleColumnsMenu" type="button" class="logs-columns-toggle" aria-haspopup="true" aria-expanded="false" title="Choose which columns to show or hide. The selection is saved in this browser.">
            <span>Columns</span>
            <span class="logs-columns-caret" aria-hidden="true">▾</span>
          </button>
          <div class="columns-menu" role="menu"></div>
        </div>
        <span id="logs-connection-status" class="logs-connection-badge disconnected">🔴 disconnected</span>
        <button id="logs-recording-toggle" class="logs-recording-toggle" type="button" aria-pressed="false" title="When ON, the server saves full request/response bodies and headers for every request (disk). When OFF, only metadata is kept.">
          <span class="logs-recording-dot" aria-hidden="true"></span>
          <span class="logs-recording-label">⏺ Record: <strong>OFF</strong></span>
        </button>
      </div>
    </div>
    <div class="logs" id="logs">
      <div class="empty" style="padding:2rem;">No recent requests yet. Use the API to see logs appear here in real time.</div>
    </div>
  `;
  // Populate the columns menu (one checkbox per column). The menu
  // starts closed; clicking the button toggles `.open` via the
  // toggleColumnsMenu handler.
  renderColumnsMenuBody(document.querySelector(".columns-menu"));
  // Close the menu when clicking anywhere outside the menu wrapper
  // (button + popover). Bound once at mount; checks `event.target`
  // and removes `.open` if the click landed outside.
  const onDocClickForMenu = (ev: Event) => {
    const menu = document.querySelector(".columns-menu");
    if (!menu || !menu.classList.contains("open")) return;
    const wrapper = menu.closest(".columns-menu-wrapper");
    if (wrapper && wrapper.contains(ev.target as Node)) return;
    menu.classList.remove("open");
  };
  const w = window as unknown as { __logsColumnsDocClickBound?: boolean };
  if (!w.__logsColumnsDocClickBound) {
    document.addEventListener("click", onDocClickForMenu);
    w.__logsColumnsDocClickBound = true;
  }
  const recBtn = document.getElementById("logs-recording-toggle");
  if (recBtn) recBtn.addEventListener("click", () => toggleRecording());
  fetchRecordingState();
  setMessageHandler(handleLogsMessage);
  connectLogsWebSocket();
  startLogLatencyTicker();
}
