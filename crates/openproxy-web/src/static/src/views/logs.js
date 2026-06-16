// views/logs.js — live logs view. The biggest view by far;
// historical rows + in-flight placeholders, paginated, with a
// 100ms latency ticker and a recording toggle. The detail modal
// lives in components/log-detail.js to keep this file under the
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
import { renderLogDetailModal, showLogDetail, updateOpenLogDetail, hasCompleteLogDetail } from "../components/log-detail.js";

function totalPages() {
  return Math.max(1, Math.ceil(state.logs.rows.length / state.logs.rowsPerPage));
}

// ---- Visible-columns state ---------------------------------------------
// The user can hide any subset of the log-table columns. The set of
// visible column keys lives on `state.logs.visibleColumns` (a Set),
// and is persisted to localStorage as a JSON array. Default is
// "all columns visible"; an empty set is forbidden (you can't hide
// the last column).
function loadVisibleColumns() {
  const allKeys = LOG_COLUMNS.map((c) => c.key);
  let result = new Set(allKeys);
  try {
    const raw = localStorage.getItem(LOGS_VISIBLE_COLUMNS_STORAGE_KEY);
    if (raw) {
      const parsed = JSON.parse(raw);
      if (Array.isArray(parsed)) {
        const valid = parsed.filter((k) => allKeys.includes(k));
        if (valid.length > 0) result = new Set(valid);
      }
    }
  } catch (_) {
    // Corrupt localStorage value — fall back to "all visible".
    result = new Set(allKeys);
  }
  return result;
}

function saveVisibleColumns() {
  try {
    localStorage.setItem(
      LOGS_VISIBLE_COLUMNS_STORAGE_KEY,
      JSON.stringify([...state.logs.visibleColumns]),
    );
  } catch (_) { /* localStorage may be disabled — non-fatal */ }
}

function mergeLogsByDescId(existing, incoming) {
  const merged = new Map();
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
      if (merged.has(reqKey)) base = merged.get(reqKey);
    }
    merged.set(k, { ...(base || {}), ...row });
    if (row.request_id) merged.set("req:" + row.request_id, merged.get(k));
    state.logs.lastSeenId = Math.max(state.logs.lastSeenId, row.id);
  }
  const seenKeys = new Set();
  let result = Array.from(merged.values()).filter((r) => {
    const key = r.id != null ? Number(r.id) : (r.request_id ? "r:" + r.request_id : Symbol());
    if (key === Symbol() || seenKeys.has(key)) return false;
    seenKeys.add(key);
    return true;
  }).sort((a, b) => (b.id || 0) - (a.id || 0));
  const limit = state.logs.maxRows;
  if (result.length > limit) {
    const removed = result.slice(limit);
    result = result.slice(0, limit);
    for (const r of removed) {
      const k = Number(r.id) || r.id;
      merged.delete(k);
    }
    state.logs.rowById = merged;
  } else {
    state.logs.rowById = merged;
  }
  return result;
}

function attachLogRowHandlers() {
  document.querySelectorAll("#logs .log-row").forEach((row) => {
    row.addEventListener("click", () => openLogDetail(row.dataset.id, row.dataset.requestId));
  });
}

function renderLogsRows() {
  const logsEl = document.getElementById("logs");
  if (!logsEl) return;
  const inflightRows = Array.from(state.logs.inflightByRequestId.values()).map((p) => {
    const t = Date.parse(p.created_at);
    const syntheticId = isFinite(t) ? (Number.MAX_SAFE_INTEGER - t) : Number.MAX_SAFE_INTEGER;
    return Object.assign({}, p, { id: syntheticId, __inflight: true });
  });
  const rows = state.logs.rows.concat(inflightRows).sort((a, b) => (b.id || 0) - (a.id || 0));
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
    ? pageRows.map((r) => renderLogRowHtml(r, state.logs.stagesByRequestId.get(r.request_id), visibleColKeys)).join("")
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

export function logsPrevPage() {
  if (state.logs.page > 1) {
    state.logs.page--;
    if (state.logs.page === 1) state.logs.followTail = true;
    renderLogsRows();
  }
}
export function logsNextPage() {
  if (state.logs.page < totalPages()) {
    state.logs.page++;
    if (state.logs.page >= totalPages()) state.logs.followTail = false;
    renderLogsRows();
  }
}
export function logsGoPage(p) {
  const total = totalPages();
  state.logs.page = Math.max(1, Math.min(p, total));
  state.logs.followTail = (state.logs.page === 1);
  renderLogsRows();
}
export function logsSetFollow(e) {
  const enabled = e && e.target ? !!e.target.checked : false;
  state.logs.followTail = enabled;
  if (enabled) { state.logs.page = 1; renderLogsRows(); }
}

// ---- Columns menu ------------------------------------------------------
// Render the popover menu body (one checkbox per column). Called
// both at mount time and after each toggle so the checked state
// stays in sync with state.logs.visibleColumns.
function renderColumnsMenuBody(menuEl) {
  if (!menuEl) return;
  const visible = state.logs.visibleColumns || new Set(LOG_COLUMNS.map((c) => c.key));
  menuEl.innerHTML = LOG_COLUMNS.map((c) => {
    const checked = visible.has(c.key) ? "checked" : "";
    // data-action="toggleColumn" data-arg1="<key>" — the global
    // click shim in app.js dispatches this to HANDLERS.toggleColumn.
    return `<label class="columns-menu-item"><input type="checkbox" data-action="toggleColumn" data-arg1="${c.key}" ${checked}><span>${escapeHtml(c.label)}</span></label>`;
  }).join("");
}

export function toggleColumnsMenu() {
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

export function toggleColumn(key) {
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
      const cb = document.querySelector(`.columns-menu input[data-arg1="${key}"]`);
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
  const cb = document.querySelector(`.columns-menu input[data-arg1="${key}"]`);
  if (cb) cb.checked = set.has(key);
  renderLogsRows();
}

function handleStageEvent(event) {
  if (!event || !event.request_id) return;
  const requestId = event.request_id;
  state.logs.stagesByRequestId.set(requestId, event);
  const existingRow = state.logs.rows.find((r) => r.request_id === requestId);
  if (existingRow) {
    if (existingRow.id != null) state.logs.rowById.set(existingRow.id, existingRow);
    if (state.logs.followTail) state.logs.page = 1;
    renderLogsRows();
    updateOpenLogDetail(existingRow);
    return;
  }
  if (!state.logs.inflightByRequestId.has(requestId)) {
    state.logs.inflightByRequestId.set(requestId, {
      id: "inflight-" + requestId,
      request_id: requestId,
      provider_id: event.provider_id || "",
      upstream_model_id: event.upstream_model_id || "",
      created_at: event.timestamp || new Date().toISOString(),
      status_code: 0, prompt_tokens: null, completion_tokens: null,
      total_ms: 0, cost_usd: 0, is_streaming: false, stream_complete: false, race_lost: false,
    });
  }
  if (state.logs.followTail) state.logs.page = 1;
  renderLogsRows();
}

function handleStreamTokens(msg) {
  const requestId = msg.request_id;
  if (!requestId) return;
  const prev = state.logs.liveTokens.get(requestId) || "";
  const next = prev + (msg.delta || "");
  state.logs.liveTokens.set(requestId, next);
  if (msg.complete) {
    const row = state.logs.rowById.get(msg.id) || state.logs.rows.find((r) => r.request_id === requestId);
    if (row) { row.stream_complete = true; renderLogsRows(); }
  }
  const panel = document.querySelector('[data-token-panel="' + cssEscape(requestId) + '"]');
  const body = document.getElementById("stream-token-body");
  if (panel) panel.textContent = next;
  if (body) { body.textContent = next; body.scrollTop = body.scrollHeight; }
}

function handleLogsMessage(raw) {
  let msg;
  try { msg = JSON.parse(raw.data); }
  catch (_) { showToast("Live Logs received an invalid WebSocket message.", "error"); return; }
  window.__logMsgTrace = window.__logMsgTrace || [];
  window.__logMsgTrace.push({ t: Date.now(), type: msg.type, hasData: !!msg.data, hasRow: !!msg.row, keys: Object.keys(msg || {}).slice(0, 10) });
  if (msg.type === "history") {
    const rows = Array.isArray(msg.rows) ? msg.rows : [];
    state.logs.rows = mergeLogsByDescId(state.logs.rows, rows);
    // Historical rows are by definition finished (they came from
    // the DB, not a live stream). Mark each one as completed/failed
    // in the stage map so the latency ticker doesn't keep ticking
    // on them when the user scrolls the page.
    for (const r of rows) {
      if (!r || !r.request_id) continue;
      if ((!r.is_streaming || r.stream_complete) && r.status_code > 0) {
        state.logs.stagesByRequestId.set(r.request_id, {
          request_id: r.request_id,
          stage: r.status_code >= 400 ? "failed" : "completed",
          elapsed_ms: r.total_ms || 0,
          status_code: r.status_code,
          timestamp: r.created_at || new Date().toISOString(),
        });
      }
    }
    state.logs.page = 1; state.logs.followTail = true; renderLogsRows();
  } else if (msg.type === "row") {
    const row = msg.data || msg.row || msg;
    state.logs.rows = mergeLogsByDescId(state.logs.rows, [row]);
    if (row.request_id && state.logs.inflightByRequestId.has(row.request_id)) {
      state.logs.inflightByRequestId.delete(row.request_id);
    }
    if (row.is_streaming && !row.stream_complete) {
      state.logs.liveTokens.set(row.request_id, state.logs.liveTokens.get(row.request_id) || "");
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
    // entry forever, and the latency ticker (state/ticker.js) keeps
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
        state.logs.stagesByRequestId.set(row.request_id, {
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
        });
      }
    }
    if (state.logs.followTail) state.logs.page = 1;
    renderLogsRows();
    updateOpenLogDetail(row);
  } else if (msg.type === "stage") {
    handleStageEvent(msg.data || msg);
  } else if (msg.type === "stream_tokens") {
    handleStreamTokens(msg);
  } else if (msg.type === "error") {
    showToast(msg.message || "Live Logs WebSocket error", "error");
  }
}

async function openLogDetail(id, requestId) {
  let row = state.logs.rowById.get(Number(id)) || state.logs.rows.find((r) => r.request_id === requestId);
  if (!row) {
    row = { id, request_id: requestId };
    state.logs.rows = mergeLogsByDescId(state.logs.rows, [row]);
  }
  state.logs.selectedRow = row;
  showLogDetail(row);
  if (!hasCompleteLogDetail(row)) {
    const detailEl = document.getElementById("log-detail-loading");
    if (detailEl) detailEl.textContent = "Loading detail…";
    try {
      const detail = await api(`/usage/detail?id=${encodeURIComponent(id)}`);
      const fetched = detail?.row || detail?.detail || detail;
      if (fetched) {
        row = { ...row, ...fetched };
        state.logs.rowById.set(Number(row.id || id), row);
        state.logs.selectedRow = row;
        // Re-render the open modal in place with the freshly fetched
        // detail data. updateOpenLogDetail handles the DOM swap
        // (it no-ops if the modal is closed, but in openLogDetail
        // we know the modal is open because we called showLogDetail
        // a few lines above).
        updateOpenLogDetail(row);
      }
    } catch (e) {
      if (detailEl) detailEl.textContent = `Detail unavailable: ${e.message || e}`;
      showToast(`Request detail unavailable: ${e.message || e}`, "error");
    }
  }
}
if (typeof window !== "undefined") window.openLogDetail = openLogDetail;

export async function mountLogs() {
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
  const onDocClickForMenu = (ev) => {
    const menu = document.querySelector(".columns-menu");
    if (!menu || !menu.classList.contains("open")) return;
    const wrapper = menu.closest(".columns-menu-wrapper");
    if (wrapper && wrapper.contains(ev.target)) return;
    menu.classList.remove("open");
  };
  if (!window.__logsColumnsDocClickBound) {
    document.addEventListener("click", onDocClickForMenu);
    window.__logsColumnsDocClickBound = true;
  }
  const recBtn = document.getElementById("logs-recording-toggle");
  if (recBtn) recBtn.addEventListener("click", () => toggleRecording());
  fetchRecordingState();
  setMessageHandler(handleLogsMessage);
  connectLogsWebSocket();
  startLogLatencyTicker();
}
