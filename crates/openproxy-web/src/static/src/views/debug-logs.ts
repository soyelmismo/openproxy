// views/debug-logs.ts — Debug Logs viewer (lit-html).
//
// Polls `/admin/debug/logs` every 2s via chained `setTimeout` (NOT
// `setInterval` — a slow request can't pile up because the next
// timer is only scheduled after the previous fetch resolves).
// Maintains a `sinceSeq` cursor that advances to `latest_seq` after
// each successful poll so we only fetch new entries.
//
// MIGRATED to lit-html: the polling loop now calls `requestUpdate()`
// instead of rebuilding the tbody via `innerHTML`. lit-html diffs the
// new entries against the previous render and patches only the rows
// that changed — filter inputs keep their value, focus is preserved,
// and the buffer indicator updates in place.
//
// Filters (Level checkboxes + Request ID / Trace ID text inputs)
// bump an epoch counter (so any in-flight poll's response is
// discarded), reset the cursor + entry list, and kick off an
// immediate poll so the new filter applies instantly.
//
// The view returns a cleanup function that cancels the pending
// poll timer; the router calls it before mounting the next view.

import { html, type TemplateResult } from 'lit-html';
import { fetchDebugLogs, clearDebugLogs } from "../lib/api.js";
import type { FetchDebugLogsOpts } from "../lib/api.js";
import { showToast } from "../components/toast.js";
import { mountView, requestUpdate } from "../state/reactive.js";
import type { DebugLogEntry } from "../lib/types/api.js";

// Poll interval. Chained via setTimeout — see `pollNow` below.
const POLL_INTERVAL_MS: number = 2000;

// Ring-buffer capacity on the server side. Used for the "Buffer:
// X / 1000" indicator. Matches `BUFFER_CAPACITY` in
// `crates/openproxy-server/src/debug_log.rs:59`.
const BUFFER_CAPACITY: number = 1000;

// Cap on the number of rows we keep in the DOM. The server's ring
// buffer holds 1000; we render at most MAX_ROWS of the most recent
// to keep the table responsive.
const MAX_ROWS: number = 500;

// Levels rendered as filter checkboxes. ERROR-first so the most
// actionable levels sit closest to the label.
const LEVELS: readonly string[] = ["ERROR", "WARN", "INFO", "DEBUG"];

// Map a level string to a CSS color value (a var() reference so the
// view adapts to light/dark themes).
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

// Clipboard write with a legacy fallback for non-secure contexts
// (where `navigator.clipboard` is unavailable — e.g. plain HTTP).
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

// Serialize the visible entries as a Markdown table for the "Copy all"
// button. Pipes and newlines in cell values are escaped so the table
// layout is preserved.
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

// ---- Module-local state (captured by the closures below) ----
//
// `stopped` is set true by the cleanup function so any in-flight
// poll can short-circuit. `epoch` is a monotonic counter bumped on
// every filter change — a poll whose epoch no longer matches the
// current one discards its response (the filter change already
// kicked off a fresh poll with the new params).
let entries: DebugLogEntry[] = [];
let sinceSeq: number = 0;
let pollHandle: ReturnType<typeof setTimeout> | null = null;
let stopped: boolean = false;
let epoch: number = 0;
let lastTotalInBuffer: number = 0;
let lastPollFailed: boolean = false;
// Error message from the last failed poll. Shown in the tbody only
// when the entry list is empty (matches the previous behaviour —
// if we already have entries, keep them visible and just toast).
let pollErrorMessage: string | null = null;

// Filter state. Updated by the @change handlers on the filter
// inputs; read by the poll loop to build the query string.
let filterLevels: Set<string> = new Set();
let filterRequestId: string = "";
let filterTraceId: string = "";

// ---- Handlers ----

async function onCopyCell(val: string): Promise<void> {
  if (!val) return;
  const ok: boolean = await copyToClipboard(val);
  if (ok) showToast(`Copied: ${val}`, "success");
  else showToast("Clipboard write failed (browser blocked it).", "error");
}

async function onCopyAll(): Promise<void> {
  if (entries.length === 0) {
    showToast("No entries to copy.", "info");
    return;
  }
  // Newest-first to match the on-screen order.
  const md: string = buildMarkdown(entries.slice().reverse());
  const ok: boolean = await copyToClipboard(md);
  if (ok) showToast(`Copied ${entries.length} entries as Markdown.`, "success");
  else showToast("Clipboard write failed (browser blocked it).", "error");
}

async function onClear(): Promise<void> {
  try {
    await clearDebugLogs();
    entries = [];
    sinceSeq = 0;
    lastTotalInBuffer = 0;
    pollErrorMessage = null;
    requestUpdate();
    showToast("Debug log buffer cleared on the server.", "success");
  } catch (e: unknown) {
    const msg: string = e instanceof Error ? e.message : String(e);
    showToast(`Failed to clear buffer: ${msg}`, "error");
  }
}

function onLevelToggle(lvl: string, e: Event): void {
  const target = e.target;
  const checked: boolean = target instanceof HTMLInputElement ? target.checked : false;
  if (checked) filterLevels.add(lvl);
  else filterLevels.delete(lvl);
  onFilterChange();
}

function onRidChange(e: Event): void {
  const target = e.target;
  filterRequestId = target instanceof HTMLInputElement ? target.value.trim() : "";
  onFilterChange();
}

function onTidChange(e: Event): void {
  const target = e.target;
  filterTraceId = target instanceof HTMLInputElement ? target.value.trim() : "";
  onFilterChange();
}

// Changing any filter cancels the pending poll timer, bumps the
// epoch (so any in-flight poll's response is discarded), resets
// the cursor + entries, and kicks off an immediate poll with the
// new params.
function onFilterChange(): void {
  epoch++;
  sinceSeq = 0;
  entries = [];
  pollErrorMessage = null;
  if (pollHandle !== null) {
    clearTimeout(pollHandle);
    pollHandle = null;
  }
  requestUpdate();
  void pollNow();
}

// ---- Poll loop ----
//
// Chained setTimeout: the next poll is scheduled only AFTER the
// current fetch resolves (success or failure). This prevents
// request pile-up when the server is slow. Declared as a hoisted
// `function` so onFilterChange (above) can reference it before its
// textual position.
async function pollNow(): Promise<void> {
  if (stopped) return;
  const myEpoch: number = epoch;
  try {
    // Build the query opts conditionally so absent filters are
    // omitted (not sent as `undefined`). Under
    // `exactOptionalPropertyTypes`, assigning a string to an
    // optional string field is allowed; assigning `undefined`
    // is not.
    const opts: FetchDebugLogsOpts = { since: sinceSeq };
    const lvl: string = Array.from(filterLevels).join(",");
    if (lvl) opts.level = lvl;
    if (filterRequestId) opts.request_id = filterRequestId;
    if (filterTraceId) opts.trace_id = filterTraceId;
    const resp = await fetchDebugLogs(opts);
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
    pollErrorMessage = null;
    requestUpdate();
  } catch (e: unknown) {
    if (stopped || myEpoch !== epoch) return;
    const msg: string = e instanceof Error ? e.message : String(e);
    // Show the error inline only when the table is empty — if we
    // already have entries, keep them visible and just toast.
    if (entries.length === 0) {
      pollErrorMessage = msg;
    }
    // Suppress repeated toasts for consecutive failures — only
    // toast on the first failure of a run so the operator isn't
    // spammed every 2s while the server is down.
    if (!lastPollFailed) {
      showToast(`Debug logs poll failed: ${msg}`, "error");
      lastPollFailed = true;
    }
    requestUpdate();
  } finally {
    // Schedule the next poll. Only when the epoch still matches
    // — if a filter change bumped the epoch, the new poll (kicked
    // off by `onFilterChange`) is responsible for rescheduling.
    if (!stopped && myEpoch === epoch) {
      pollHandle = setTimeout(() => { void pollNow(); }, POLL_INTERVAL_MS);
    }
  }
}

// ---- Templates ----

// Build a single table row for an entry. The request_id and
// trace_id cells are rendered as <button> elements so they're
// keyboard-focusable and announce as interactive; the @click
// handler carries the value to copy.
function renderRow(entry: DebugLogEntry): TemplateResult {
  const lvlColor: string = levelColor(entry.level);
  const rid: string | null = entry.request_id;
  const tid: string | null = entry.trace_id;
  const ridCell: TemplateResult = rid
    ? html`<button type="button" class="debug-copyable" title="Click to copy request ID" @click=${() => onCopyCell(rid)}>${rid}</button>`
    : html`<span class="muted">—</span>`;
  const tidCell: TemplateResult = tid
    ? html`<button type="button" class="debug-copyable" title="Click to copy trace ID" @click=${() => onCopyCell(tid)}>${tid}</button>`
    : html`<span class="muted">—</span>`;
  const spanPath: TemplateResult = entry.span_path
    ? html`<br><small class="muted debug-span-path" title=${entry.span_path}>${entry.span_path}</small>`
    : html``;
  return html`<tr>
    <td class="debug-time" title=${entry.timestamp} style="white-space:nowrap;font-family:var(--font-mono);font-size:0.8rem;">${formatTime(entry.timestamp)}</td>
    <td class="debug-level" style="color:${lvlColor};font-weight:600;white-space:nowrap;">${entry.level}</td>
    <td class="debug-target" title=${entry.target} style="font-family:var(--font-mono);font-size:0.8rem;">${entry.target}</td>
    <td class="debug-rid">${ridCell}</td>
    <td class="debug-tid">${tidCell}</td>
    <td class="debug-message" style="word-break:break-word;">${entry.message}${spanPath}</td>
  </tr>`;
}

function renderTbody(): TemplateResult {
  if (entries.length === 0) {
    const msg: string = pollErrorMessage ? `Poll error: ${pollErrorMessage}` : "No debug log entries yet.";
    return html`<tr><td colspan="6" class="empty" style="text-align:center;padding:1rem;color:var(--color-text-muted);">${msg}</td></tr>`;
  }
  // Server returns oldest-first; we show newest-first (reverse)
  // and cap at MAX_ROWS so the DOM doesn't grow unbounded.
  const rows: DebugLogEntry[] = entries.slice().reverse().slice(0, MAX_ROWS);
  return html`${rows.map(renderRow)}`;
}

function renderLevelChecks(): TemplateResult {
  return html`${LEVELS.map((lvl: string) => {
    const color: string = levelColor(lvl);
    const id: string = `debug-level-filter-${lvl.toLowerCase()}`;
    return html`<label class="debug-level-check" style="display:inline-flex;align-items:center;gap:0.25rem;margin-right:0.5rem;cursor:pointer;">
      <input type="checkbox" id=${id} class="debug-level-filter" value=${lvl} ?checked=${filterLevels.has(lvl)} @change=${(e: Event) => onLevelToggle(lvl, e)}>
      <span style="color:${color};font-weight:600;font-size:0.8rem;">${lvl}</span>
    </label>`;
  })}`;
}

function renderDebugLogs(): TemplateResult {
  return html`
    <div class="page-header"><h2>Debug Logs</h2>
      <div class="actions">
        <span class="debug-buffer-indicator" id="debug-buffer-indicator" style="font-size:0.85rem;color:var(--color-text-muted);margin-right:0.5rem;font-family:var(--font-mono);">Buffer: ${lastTotalInBuffer} / ${BUFFER_CAPACITY}</span>
        <button type="button" class="link" title="Copy all visible entries as Markdown to the clipboard" @click=${onCopyAll}>Copy all</button>
        <button type="button" class="link" title="Clear the in-memory debug log ring buffer on the server" style="margin-left:0.5rem;" @click=${onClear}>Clear</button>
      </div>
    </div>
    <div class="debug-filters" style="display:flex;flex-wrap:wrap;gap:1rem;align-items:flex-end;padding:0.5rem 0 1rem;border-bottom:1px solid var(--color-border-soft);margin-bottom:0.5rem;">
      <div class="debug-filter-group" style="display:flex;flex-direction:column;gap:0.25rem;">
        <span class="debug-filter-label" style="font-size:0.72rem;text-transform:uppercase;color:var(--color-text-muted);">Level</span>
        <div>${renderLevelChecks()}</div>
      </div>
      <div class="debug-filter-group" style="display:flex;flex-direction:column;gap:0.25rem;">
        <label class="debug-filter-label" for="debug-filter-request-id" style="font-size:0.72rem;text-transform:uppercase;color:var(--color-text-muted);">Request ID</label>
        <input type="text" id="debug-filter-request-id" placeholder="req-abc123" autocomplete="off" style="padding:0.25rem 0.5rem;border:1px solid var(--color-border-soft);min-width:12rem;background:var(--color-surface);color:var(--color-text);" @change=${onRidChange}>
      </div>
      <div class="debug-filter-group" style="display:flex;flex-direction:column;gap:0.25rem;">
        <label class="debug-filter-label" for="debug-filter-trace-id" style="font-size:0.72rem;text-transform:uppercase;color:var(--color-text-muted);">Trace ID</label>
        <input type="text" id="debug-filter-trace-id" placeholder="tr-def456" autocomplete="off" style="padding:0.25rem 0.5rem;border:1px solid var(--color-border-soft);min-width:12rem;background:var(--color-surface);color:var(--color-text);" @change=${onTidChange}>
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
        <tbody id="debug-logs-tbody">${renderTbody()}</tbody>
      </table>
    </div>
  `;
}

// ---- Mount ----
//
// Mount the Debug Logs view into `container`. Renders the header,
// filter bar, and entries table; starts the 2s polling loop; and
// returns a cleanup function that cancels the pending poll timer.
//
// The cleanup function is called by the router before the next
// view mounts, so the polling loop doesn't leak across navigations.
export function mountDebugLogs(container: HTMLElement): () => void {
  // Reset module-local state on every mount — the router calls
  // cleanup of the previous view before mounting this one, so
  // `stopped` is true coming in; flip it back to false here.
  entries = [];
  sinceSeq = 0;
  pollHandle = null;
  stopped = false;
  epoch = 0;
  lastTotalInBuffer = 0;
  lastPollFailed = false;
  pollErrorMessage = null;
  filterLevels = new Set();
  filterRequestId = "";
  filterTraceId = "";

  const cleanupReactive = mountView(container, renderDebugLogs);

  // Kick off the first poll immediately (no 2s delay on mount).
  void pollNow();

  // Cleanup: cancel the pending poll timer. The in-flight fetch
  // (if any) will short-circuit on the `stopped` check when it
  // resolves. Also release the lit-html container so the next
  // view's mountView doesn't race with our requestUpdate().
  return () => {
    stopped = true;
    if (pollHandle !== null) {
      clearTimeout(pollHandle);
      pollHandle = null;
    }
    cleanupReactive();
  };
}
