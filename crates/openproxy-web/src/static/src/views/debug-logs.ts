// views/debug-logs.ts — Debug Logs viewer.
//
// Polls `/admin/debug/logs` every 2s via chained `setTimeout` (NOT
// `setInterval` — a slow request can't pile up because the next
// timer is only scheduled after the previous fetch resolves).
// Maintains a `sinceSeq` cursor that advances to `latest_seq` after
// each successful poll so we only fetch new entries.
//
// Filters (Level checkboxes + Request ID / Trace ID text inputs)
// are read on every poll and sent as query params. Changing any
// filter cancels the pending poll timer, bumps an epoch counter
// (so any in-flight poll's response is discarded), resets the
// cursor + the local entry list, and kicks off an immediate poll
// so the new filter applies instantly.
//
// The view returns a cleanup function that cancels the pending
// poll timer; the router calls it before mounting the next view.

import { fetchDebugLogs, clearDebugLogs } from "../lib/api.js";
import type { FetchDebugLogsOpts } from "../lib/api.js";
import { escapeHtml, escapeAttr } from "../lib/escape.js";
import { showToast } from "../components/toast.js";
import { pageHeader } from "../components/page-header.js";
import type { DebugLogEntry, DebugLogsResponse } from "../lib/types/api.js";

// Poll interval. Chained via setTimeout — see `pollNow` below.
const POLL_INTERVAL_MS: number = 2000;

// Ring-buffer capacity on the server side. Used for the "Buffer:
// X / 1000" indicator. Matches `BUFFER_CAPACITY` in
// `crates/openproxy-server/src/debug_log.rs:59`.
const BUFFER_CAPACITY: number = 1000;

// Cap on the number of rows we keep in the DOM. The server's ring
// buffer holds 1000; we render at most MAX_ROWS of the most recent
// to keep the table responsive. Older entries are still in the
// server buffer and can be fetched by narrowing the filter.
const MAX_ROWS: number = 500;

// Levels rendered as filter checkboxes. The backend's tracing layer
// emits these (plus TRACE, which we omit — too noisy for the
// dashboard). ERROR-first so the most actionable levels sit closest
// to the label.
const LEVELS: readonly string[] = ["ERROR", "WARN", "INFO", "DEBUG"];

// Map a level string to a CSS color value (a var() reference so the
// view adapts to light/dark themes). Unknown levels fall back to
// the muted text color.
function levelColor(level: string): string {
  const upper: string = level.toUpperCase();
  if (upper === "ERROR") return "var(--color-error)";
  if (upper === "WARN") return "var(--color-warn)";
  if (upper === "INFO") return "var(--color-info)";
  if (upper === "DEBUG") return "var(--color-text-muted)";
  return "var(--color-text-muted)";
}

// Strip the date portion from an ISO-8601 timestamp for compact
// display in the table; the full timestamp is preserved in the
// `title` attribute so hover shows the complete value.
function formatTime(ts: string): string {
  const t: string = ts || "";
  const idx: number = t.indexOf("T");
  if (idx < 0) return t;
  return t.slice(idx + 1).replace(/Z$/, "");
}

interface FilterState {
  levels: Set<string>;
  requestId: string;
  traceId: string;
}

// Read the filter inputs from the DOM. Called on every poll.
// Defensive: returns empty strings / empty set when the inputs
// don't exist (e.g. the view was unmounted mid-read).
function readFilters(container: HTMLElement): FilterState {
  const levels: Set<string> = new Set();
  container.querySelectorAll<HTMLInputElement>("input.debug-level-filter:checked")
    .forEach((cb: HTMLInputElement) => {
      const v: string = cb.value;
      if (v) levels.add(v);
    });
  const ridEl: HTMLInputElement | null = container.querySelector<HTMLInputElement>("#debug-filter-request-id");
  const tidEl: HTMLInputElement | null = container.querySelector<HTMLInputElement>("#debug-filter-trace-id");
  return {
    levels,
    requestId: ridEl ? ridEl.value.trim() : "",
    traceId: tidEl ? tidEl.value.trim() : "",
  };
}

function buildLevelParam(levels: Set<string>): string {
  return Array.from(levels).join(",");
}

// Build a single table row for an entry. The request_id and trace_id
// cells are rendered as <button> elements so they're keyboard-focusable
// and announce as interactive; `data-copy` carries the value to copy.
function renderRow(entry: DebugLogEntry): string {
  const time: string = escapeHtml(formatTime(entry.timestamp));
  const fullTime: string = escapeAttr(entry.timestamp);
  const level: string = escapeHtml(entry.level);
  const lvlColor: string = levelColor(entry.level);
  const target: string = escapeHtml(entry.target);
  const message: string = escapeHtml(entry.message);
  const rid: string | null = entry.request_id;
  const tid: string | null = entry.trace_id;
  const ridCell: string = rid
    ? `<button type="button" class="debug-copyable" data-copy="${escapeAttr(rid)}" title="Click to copy request ID">${escapeHtml(rid)}</button>`
    : `<span class="muted">—</span>`;
  const tidCell: string = tid
    ? `<button type="button" class="debug-copyable" data-copy="${escapeAttr(tid)}" title="Click to copy trace ID">${escapeHtml(tid)}</button>`
    : `<span class="muted">—</span>`;
  const spanPath: string = entry.span_path
    ? `<br><small class="muted debug-span-path" title="${escapeAttr(entry.span_path)}">${escapeHtml(entry.span_path)}</small>`
    : "";
  return `<tr>
    <td class="debug-time" title="${fullTime}" style="white-space:nowrap;font-family:var(--font-mono);font-size:0.8rem;">${time}</td>
    <td class="debug-level" style="color:${lvlColor};font-weight:600;white-space:nowrap;">${level}</td>
    <td class="debug-target" title="${escapeAttr(entry.target)}" style="font-family:var(--font-mono);font-size:0.8rem;">${target}</td>
    <td class="debug-rid">${ridCell}</td>
    <td class="debug-tid">${tidCell}</td>
    <td class="debug-message" style="word-break:break-word;">${message}${spanPath}</td>
  </tr>`;
}

function renderEmpty(message: string): string {
  return `<tr><td colspan="6" class="empty" style="text-align:center;padding:1rem;color:var(--color-text-muted);">${escapeHtml(message)}</td></tr>`;
}

// Serialize the visible entries as a Markdown table for the "Copy all"
// button. Pipes and newlines in cell values are escaped so the table
// layout is preserved. The timestamp is included verbatim (ISO-8601).
// Entries are rendered newest-first to match the on-screen order.
function buildMarkdown(rows: DebugLogEntry[]): string {
  const lines: string[] = [];
  lines.push("# Debug Logs");
  lines.push("");
  lines.push(`_Exported ${new Date().toISOString()} — ${rows.length} entries_`);
  lines.push("");
  lines.push("| Time | Level | Target | Request ID | Trace ID | Message |");
  lines.push("| --- | --- | --- | --- | --- | --- |");
  const esc = (s: string): string => s.replace(/\|/g, "\\|").replace(/\r?\n/g, " ");
  for (const r of rows) {
    const time: string = r.timestamp || "";
    const level: string = r.level || "";
    const target: string = esc(r.target || "");
    const rid: string = esc(r.request_id || "");
    const tid: string = esc(r.trace_id || "");
    const msg: string = esc(r.message || "");
    lines.push(`| ${time} | ${level} | ${target} | ${rid} | ${tid} | ${msg} |`);
  }
  return lines.join("\n");
}

// Clipboard write with a legacy fallback for non-secure contexts
// (where `navigator.clipboard` is unavailable — e.g. plain HTTP).
// Returns true on success, false on failure.
async function copyToClipboard(text: string): Promise<boolean> {
  try {
    if (typeof navigator !== "undefined" && navigator.clipboard && navigator.clipboard.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch (_e: unknown) {
    // Fall through to the legacy fallback.
  }
  try {
    const ta: HTMLTextAreaElement = document.createElement("textarea");
    ta.value = text;
    ta.style.position = "fixed";
    ta.style.left = "-9999px";
    ta.setAttribute("readonly", "");
    document.body.appendChild(ta);
    ta.select();
    const ok: boolean = document.execCommand("copy");
    document.body.removeChild(ta);
    return ok;
  } catch (_e: unknown) {
    return false;
  }
}

/**
 * Mount the Debug Logs view into `container`. Renders the header,
 * filter bar, and entries table; starts the 2s polling loop; and
 * returns a cleanup function that cancels the pending poll timer.
 *
 * The cleanup function is called by the router before the next
 * view mounts, so the polling loop doesn't leak across navigations.
 */
export function mountDebugLogs(container: HTMLElement): () => void {
  // ── Local mutable state ───────────────────────────────────────
  // Captured by the closures below. `stopped` is set true by the
  // cleanup function so any in-flight poll can short-circuit.
  let entries: DebugLogEntry[] = [];
  let sinceSeq: number = 0;
  let pollHandle: ReturnType<typeof setTimeout> | null = null;
  let stopped: boolean = false;
  // Monotonic epoch incremented on every filter change. A poll whose
  // epoch no longer matches the current one discards its response —
  // the filter change already kicked off a fresh poll with the new
  // params. Without this, a slow in-flight poll could overwrite the
  // entry list with stale (unfiltered) data right after the user
  // typed a filter.
  let epoch: number = 0;
  let lastTotalInBuffer: number = 0;
  let lastPollFailed: boolean = false;

  // ── Render: shell (header + filters + empty table) ────────────
  // Built once at mount; subsequent updates only touch the tbody
  // and the buffer indicator so filter inputs don't lose focus.
  const levelChecks: string = LEVELS.map((lvl: string) => {
    const id: string = `debug-level-filter-${lvl.toLowerCase()}`;
    const color: string = levelColor(lvl);
    return `<label class="debug-level-check" style="display:inline-flex;align-items:center;gap:0.25rem;margin-right:0.5rem;cursor:pointer;">
      <input type="checkbox" id="${id}" class="debug-level-filter" value="${escapeAttr(lvl)}">
      <span style="color:${color};font-weight:600;font-size:0.8rem;">${escapeHtml(lvl)}</span>
    </label>`;
  }).join("");

  container.innerHTML = pageHeader({
    title: "Debug Logs",
    actions:
      `<span class="debug-buffer-indicator" id="debug-buffer-indicator" style="font-size:0.85rem;color:var(--color-text-muted);margin-right:0.5rem;font-family:var(--font-mono);">Buffer: — / ${BUFFER_CAPACITY}</span>` +
      `<button type="button" id="debug-copy-btn" class="link" title="Copy all visible entries as Markdown to the clipboard">Copy all</button>` +
      `<button type="button" id="debug-clear-btn" class="link" title="Clear the in-memory debug log ring buffer on the server" style="margin-left:0.5rem;">Clear</button>`,
  }) + `
    <div class="debug-filters" style="display:flex;flex-wrap:wrap;gap:1rem;align-items:flex-end;padding:0.5rem 0 1rem;border-bottom:1px solid var(--color-border-soft);margin-bottom:0.5rem;">
      <div class="debug-filter-group" style="display:flex;flex-direction:column;gap:0.25rem;">
        <span class="debug-filter-label" style="font-size:0.72rem;text-transform:uppercase;color:var(--color-text-muted);">Level</span>
        <div>${levelChecks}</div>
      </div>
      <div class="debug-filter-group" style="display:flex;flex-direction:column;gap:0.25rem;">
        <label class="debug-filter-label" for="debug-filter-request-id" style="font-size:0.72rem;text-transform:uppercase;color:var(--color-text-muted);">Request ID</label>
        <input type="text" id="debug-filter-request-id" placeholder="req-abc123" autocomplete="off" style="padding:0.25rem 0.5rem;border:1px solid var(--color-border-soft);min-width:12rem;background:var(--color-surface);color:var(--color-text);">
      </div>
      <div class="debug-filter-group" style="display:flex;flex-direction:column;gap:0.25rem;">
        <label class="debug-filter-label" for="debug-filter-trace-id" style="font-size:0.72rem;text-transform:uppercase;color:var(--color-text-muted);">Trace ID</label>
        <input type="text" id="debug-filter-trace-id" placeholder="tr-def456" autocomplete="off" style="padding:0.25rem 0.5rem;border:1px solid var(--color-border-soft);min-width:12rem;background:var(--color-surface);color:var(--color-text);">
      </div>
    </div>
    <div class="debug-table-wrap" style="overflow-x:auto;">
      <table class="debug-logs-table" style="width:100%;border-collapse:collapse;font-size:0.85rem;">
        <thead>
          <tr style="border-bottom:2px solid var(--color-border);text-align:left;">
            <th class="debug-time" style="padding:0.35rem 0.5rem;white-space:nowrap;">Time</th>
            <th class="debug-level" style="padding:0.35rem 0.5rem;">Level</th>
            <th class="debug-target" style="padding:0.35rem 0.5rem;">Target</th>
            <th class="debug-rid" style="padding:0.35rem 0.5rem;">Request ID</th>
            <th class="debug-tid" style="padding:0.35rem 0.5rem;">Trace ID</th>
            <th class="debug-message" style="padding:0.35rem 0.5rem;">Message</th>
          </tr>
        </thead>
        <tbody id="debug-logs-tbody">
          ${renderEmpty("Loading…")}
        </tbody>
      </table>
    </div>
  `;

  // ── Wire: copy-on-click for request_id / trace_id cells ───────
  // Re-bound after every table re-render because the rows are
  // recreated via innerHTML. Called from `renderTableBody`.
  const wireCopyableCells = (): void => {
    container.querySelectorAll<HTMLButtonElement>("button.debug-copyable").forEach((btn: HTMLButtonElement) => {
      btn.addEventListener("click", async (ev: MouseEvent) => {
        ev.stopPropagation();
        const val: string = btn.dataset["copy"] || "";
        if (!val) return;
        const ok: boolean = await copyToClipboard(val);
        if (ok) {
          showToast(`Copied: ${val}`, "success");
        } else {
          showToast("Clipboard write failed (browser blocked it).", "error");
        }
      });
    });
  };

  // ── Render: table body + buffer indicator ─────────────────────
  // Called after every successful poll and after a Clear.
  const renderTableBody = (): void => {
    const tbody: HTMLTableSectionElement | null = container.querySelector<HTMLTableSectionElement>("#debug-logs-tbody");
    if (tbody) {
      if (entries.length === 0) {
        tbody.innerHTML = renderEmpty("No debug log entries yet.");
      } else {
        // Server returns oldest-first; we show newest-first (reverse)
        // and cap at MAX_ROWS so the DOM doesn't grow unbounded.
        const rows: DebugLogEntry[] = entries.slice().reverse().slice(0, MAX_ROWS);
        tbody.innerHTML = rows.map(renderRow).join("");
      }
      wireCopyableCells();
    }
    const bufEl: HTMLSpanElement | null = container.querySelector<HTMLSpanElement>("#debug-buffer-indicator");
    if (bufEl) {
      bufEl.textContent = `Buffer: ${lastTotalInBuffer} / ${BUFFER_CAPACITY}`;
    }
  };

  // ── Wire: Clear + Copy all buttons ────────────────────────────
  const clearBtn: HTMLButtonElement | null = container.querySelector<HTMLButtonElement>("#debug-clear-btn");
  if (clearBtn) {
    clearBtn.addEventListener("click", async () => {
      try {
        await clearDebugLogs();
        entries = [];
        sinceSeq = 0;
        lastTotalInBuffer = 0;
        renderTableBody();
        showToast("Debug log buffer cleared on the server.", "success");
      } catch (e: unknown) {
        const msg: string = e instanceof Error ? e.message : String(e);
        showToast(`Failed to clear buffer: ${msg}`, "error");
      }
    });
  }
  const copyBtn: HTMLButtonElement | null = container.querySelector<HTMLButtonElement>("#debug-copy-btn");
  if (copyBtn) {
    copyBtn.addEventListener("click", async () => {
      if (entries.length === 0) {
        showToast("No entries to copy.", "info");
        return;
      }
      // Newest-first to match the on-screen order.
      const md: string = buildMarkdown(entries.slice().reverse());
      const ok: boolean = await copyToClipboard(md);
      if (ok) {
        showToast(`Copied ${entries.length} entries as Markdown.`, "success");
      } else {
        showToast("Clipboard write failed (browser blocked it).", "error");
      }
    });
  }

  // ── Poll loop ─────────────────────────────────────────────────
  // Chained setTimeout: the next poll is scheduled only AFTER the
  // current fetch resolves (success or failure). This prevents
  // request pile-up when the server is slow.
  //
  // Declared as a hoisted `function` so the filter-change handler
  // (declared below) can reference it before its textual position.
  async function pollNow(): Promise<void> {
    if (stopped) return;
    const myEpoch: number = epoch;
    const filters: FilterState = readFilters(container);
    try {
      // Build the query opts conditionally so absent filters are
      // omitted (not sent as `undefined`). Under
      // `exactOptionalPropertyTypes`, assigning a string to an
      // optional string field is allowed; assigning `undefined`
      // is not.
      const opts: FetchDebugLogsOpts = { since: sinceSeq };
      const lvl: string = buildLevelParam(filters.levels);
      if (lvl) opts.level = lvl;
      if (filters.requestId) opts.request_id = filters.requestId;
      if (filters.traceId) opts.trace_id = filters.traceId;
      const resp: DebugLogsResponse = await fetchDebugLogs(opts);
      // Discard the response if a filter changed (epoch bumped) or
      // the view was unmounted while the fetch was in flight.
      if (stopped || myEpoch !== epoch) return;
      lastTotalInBuffer = resp.total_in_buffer;
      if (resp.entries.length > 0) {
        // Merge: dedupe by seq in case a poll overlap returned the
        // same entries twice (e.g. after a network blip where the
        // server processed the request but the client timed out
        // and retried).
        const seen: Set<number> = new Set(entries.map((e: DebugLogEntry) => e.seq));
        for (const e of resp.entries) {
          if (!seen.has(e.seq)) {
            entries.push(e);
            seen.add(e.seq);
          }
        }
        // Trim to MAX_ROWS (keep the newest).
        if (entries.length > MAX_ROWS) {
          entries = entries.slice(entries.length - MAX_ROWS);
        }
      }
      sinceSeq = resp.latest_seq;
      lastPollFailed = false;
      renderTableBody();
    } catch (e: unknown) {
      if (stopped || myEpoch !== epoch) return;
      const msg: string = e instanceof Error ? e.message : String(e);
      // Show the error inline only when the table is empty — if we
      // already have entries, keep them visible and just toast.
      const tbody: HTMLTableSectionElement | null = container.querySelector<HTMLTableSectionElement>("#debug-logs-tbody");
      if (tbody && entries.length === 0) {
        tbody.innerHTML = renderEmpty(`Poll error: ${msg}`);
      }
      // Suppress repeated toasts for consecutive failures — only
      // toast on the first failure of a run so the operator isn't
      // spammed every 2s while the server is down.
      if (!lastPollFailed) {
        showToast(`Debug logs poll failed: ${msg}`, "error");
        lastPollFailed = true;
      }
    } finally {
      // Schedule the next poll. Only when the epoch still matches
      // — if a filter change bumped the epoch, the new poll (kicked
      // off by `onFilterChange`) is responsible for rescheduling.
      if (!stopped && myEpoch === epoch) {
        pollHandle = setTimeout(() => { void pollNow(); }, POLL_INTERVAL_MS);
      }
    }
  }

  // ── Wire: filter inputs ───────────────────────────────────────
  // Changing any filter cancels the pending poll timer, bumps the
  // epoch (so any in-flight poll's response is discarded), resets
  // the cursor + entries, and kicks off an immediate poll with the
  // new params.
  const onFilterChange = (): void => {
    epoch++;
    sinceSeq = 0;
    entries = [];
    if (pollHandle !== null) {
      clearTimeout(pollHandle);
      pollHandle = null;
    }
    renderTableBody();
    void pollNow();
  };
  container.querySelectorAll<HTMLInputElement>("input.debug-level-filter").forEach((cb: HTMLInputElement) => {
    cb.addEventListener("change", onFilterChange);
  });
  const ridEl: HTMLInputElement | null = container.querySelector<HTMLInputElement>("#debug-filter-request-id");
  if (ridEl) ridEl.addEventListener("change", onFilterChange);
  const tidEl: HTMLInputElement | null = container.querySelector<HTMLInputElement>("#debug-filter-trace-id");
  if (tidEl) tidEl.addEventListener("change", onFilterChange);

  // Kick off the first poll immediately (no 2s delay on mount).
  void pollNow();

  // ── Cleanup ───────────────────────────────────────────────────
  // Cancel the pending poll timer. The in-flight fetch (if any)
  // will short-circuit on the `stopped` check when it resolves.
  return () => {
    stopped = true;
    if (pollHandle !== null) {
      clearTimeout(pollHandle);
      pollHandle = null;
    }
  };
}
