// views/debug-logs.ts — Debug Logs viewer.
//
// Polls `/admin/debug/logs` every 2s via chained `setTimeout` (not
// `setInterval`: a slow request cannot pile up because the next timer is
// scheduled only after the previous fetch resolves). A `sinceSeq` cursor
// advances to `latest_seq` after each successful poll so only new entries
// are fetched.
//
// Ported to renderDataGrid: one line per row (32px), elastic message column with min,
// semantic max for timestamp/level/target/ids, context menu, hover quick actions,
// and bulk actions.
//
// A filter change bumps an epoch counter (discarding any in-flight poll's
// response), resets the cursor and entry list, and starts an immediate
// poll.
//
// The cleanup function cancels the pending poll timer; the router calls it
// before mounting the next view.

import { html, type TemplateResult } from 'lit-html';
import { fetchDebugLogs, clearDebugLogs } from "../lib/api.js";
import { icons } from "../lib/icons.js";
import type { FetchDebugLogsOpts } from "../lib/api.js";
import { showToast } from "../components/toast.js";
import { copyToClipboard } from "../lib/clipboard.js";
import { mountView, requestUpdate } from "../state/reactive.js";
import type { DebugLogEntry } from "../lib/types/api.js";
import { renderDataGrid, type GridColumn } from "../components/data-grid.js";
import { t } from "../i18n/index.js";
// Mark entries viewed on every successful poll so the sidebar badge
// (driven by `state/debug-logs-store.ts`) clears while the user is on
// this page. The store's 30s poll resumes accumulating on navigation.
import { markDebugLogsViewed } from "../state/debug-logs-store.js";

// Poll interval. Chained via setTimeout — see `pollNow` below.
const POLL_INTERVAL_MS: number = 2000;

// Server-side ring-buffer capacity, shown in the "Buffer: X / 1000"
// indicator. Mirrors `BUFFER_CAPACITY` in
// `crates/openproxy-server/src/debug_log.rs`.
const BUFFER_CAPACITY: number = 1000;

// Cap on the number of rows we keep in the DOM. The server's ring
// buffer holds 1000; we render at most MAX_ROWS of the most recent
// to keep the table responsive.
const MAX_ROWS: number = 500;

// Levels rendered as filter checkboxes. ERROR-first so the most
// actionable levels sit closest to the label.
const LEVELS: readonly string[] = ["ERROR", "WARN", "INFO", "DEBUG"];

// Strip the date portion from an ISO-8601 timestamp for compact
// display in the table; the full timestamp is preserved in the
// `title` attribute so hover shows the complete value.
function formatTime(ts: string): string {
  const tStr: string = ts || "";
  const idx: number = tStr.indexOf("T");
  if (idx < 0) return tStr;
  return tStr.slice(idx + 1).replace(/Z$/, "");
}

// Returns `Promise<boolean>` so call sites can branch on success.
async function copyToClipboardOk(text: string): Promise<boolean> {
  try {
    await copyToClipboard(text);
    return true;
  } catch {
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

let entries: DebugLogEntry[] = [];
let sinceSeq: number = 0;
let pollHandle: ReturnType<typeof setTimeout> | null = null;
let stopped: boolean = false;
let epoch: number = 0;
let lastTotalInBuffer: number = 0;
let lastPollFailed: boolean = false;
let pollErrorMessage: string | null = null;

let filterLevels: Set<string> = new Set();
let filterRequestId: string = "";
let filterTraceId: string = "";

async function onCopyCell(val: string): Promise<void> {
  if (!val) return;
  const ok: boolean = await copyToClipboardOk(val);
  if (ok) showToast(`Copied: ${val}`, "success");
  else showToast("Clipboard write failed (browser blocked it).", "error");
}

async function onCopyAll(): Promise<void> {
  if (entries.length === 0) {
    showToast("No entries to copy.", "info");
    return;
  }
  const md: string = buildMarkdown(entries.slice().reverse());
  const ok: boolean = await copyToClipboardOk(md);
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
    markDebugLogsViewed();
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

async function pollNow(): Promise<void> {
  if (stopped) return;
  const myEpoch: number = epoch;
  try {
    const opts: FetchDebugLogsOpts = { since: sinceSeq };
    const lvl: string = Array.from(filterLevels).join(",");
    if (lvl) opts.level = lvl;
    if (filterRequestId) opts.request_id = filterRequestId;
    if (filterTraceId) opts.trace_id = filterTraceId;
    const resp = await fetchDebugLogs(opts);
    if (stopped || myEpoch !== epoch) return;
    lastTotalInBuffer = resp.total_in_buffer;
    if (resp.entries.length > 0) {
      const seen: Set<number> = new Set(entries.map((e: DebugLogEntry) => e.seq));
      for (const e of resp.entries) {
        if (!seen.has(e.seq)) {
          entries.push(e);
          seen.add(e.seq);
        }
      }
      if (entries.length > MAX_ROWS) {
        entries = entries.slice(entries.length - MAX_ROWS);
      }
    }
    sinceSeq = resp.latest_seq;
    lastPollFailed = false;
    pollErrorMessage = null;
    markDebugLogsViewed();
    requestUpdate();
  } catch (e: unknown) {
    if (stopped || myEpoch !== epoch) return;
    const msg: string = e instanceof Error ? e.message : String(e);
    if (entries.length === 0) {
      pollErrorMessage = msg;
    }
    if (!lastPollFailed) {
      showToast(`Debug logs poll failed: ${msg}`, "error");
      lastPollFailed = true;
    }
    requestUpdate();
  } finally {
    if (!stopped && myEpoch === epoch) {
      pollHandle = setTimeout(() => { void pollNow(); }, POLL_INTERVAL_MS);
    }
  }
}

function renderLevelChecks(): TemplateResult {
  return html`${LEVELS.map((lvl: string) => {
    const id: string = `debug-level-filter-${lvl.toLowerCase()}`;
    return html`<label class="debug-level-check cx-level-check" title="Filter ${lvl} entries">
      <input type="checkbox" id=${id} class="debug-level-filter" value=${lvl} ?checked=${filterLevels.has(lvl)} @change=${(e: Event) => onLevelToggle(lvl, e)}>
      <span class="debug-level-pill level-${lvl.toLowerCase()}">${lvl}</span>
    </label>`;
  })}`;
}

function getDebugColumns(): GridColumn<DebugLogEntry>[] {
  return [
    {
      key: "timestamp",
      label: "Time",
      max: 100,
      render: (entry) => html`<span class="debug-time-text" title=${entry.timestamp}>${formatTime(entry.timestamp)}</span>`,
    },
    {
      key: "level",
      label: "Level",
      max: 85,
      render: (entry) => {
        const lvl = (entry.level || "DEBUG").toUpperCase();
        return html`<span class="debug-level-pill level-${lvl.toLowerCase()}">${lvl}</span>`;
      },
    },
    {
      key: "target",
      label: "Target",
      max: 140,
      fold: 3,
      render: (entry) => html`<span class="debug-target-text dg-mono" title=${entry.target}>${entry.target}</span>`,
    },
    {
      key: "request_id",
      label: "Request ID",
      max: 140,
      fold: 2,
      render: (entry) => {
        const rid = entry.request_id;
        return rid
          ? html`<button type="button" class="debug-id-btn" title="Click to copy request ID: ${rid}" @click=${(e: Event) => { e.stopPropagation(); void onCopyCell(rid); }}>${rid}</button>`
          : html`<span class="dg-dim">—</span>`;
      },
    },
    {
      key: "trace_id",
      label: "Trace ID",
      max: 140,
      fold: 4,
      render: (entry) => {
        const tid = entry.trace_id;
        return tid
          ? html`<button type="button" class="debug-id-btn" title="Click to copy trace ID: ${tid}" @click=${(e: Event) => { e.stopPropagation(); void onCopyCell(tid); }}>${tid}</button>`
          : html`<span class="dg-dim">—</span>`;
      },
    },
    {
      key: "message",
      label: "Message",
      min: 150,
      elastic: true,
      render: (entry) => html`
        <span class="debug-msg-text" title=${entry.message}>${entry.message}</span>
        ${entry.span_path ? html`<small class="debug-span-path" title=${entry.span_path}>[${entry.span_path}]</small>` : ""}
      `,
    },
  ];
}

function renderDebugLogs(): TemplateResult {
  const displayRows = entries.slice().reverse().slice(0, MAX_ROWS);
  const emptyMsg = pollErrorMessage
    ? `Poll error: ${pollErrorMessage}`
    : "No debug log entries yet.";

  return html`
    <div class="cx-toolbar cx-debug-toolbar" title="Live tail of the server debug ring buffer" aria-description="Live tail of the server debug ring buffer">
      <h2 class="view-identity">Debug Logs</h2>
      <div class="cx-debug-levels" role="group" aria-label="Filter by level">${renderLevelChecks()}</div>
      <label class="cx-debug-field">
        <span class="sr-only">Request ID</span>
        <input type="text" id="debug-filter-request-id" placeholder="Request ID" autocomplete="off" @change=${onRidChange}>
      </label>
      <label class="cx-debug-field">
        <span class="sr-only">Trace ID</span>
        <input type="text" id="debug-filter-trace-id" placeholder="Trace ID" autocomplete="off" @change=${onTidChange}>
      </label>
      <span class="debug-buffer-indicator cx-debug-buffer" id="debug-buffer-indicator" title="Server ring buffer usage">Buffer: ${lastTotalInBuffer} / ${BUFFER_CAPACITY}</span>
      <div class="cx-toolbar-actions">
        <button type="button" class="link" title="Copy all visible entries as Markdown to the clipboard" @click=${onCopyAll}>${icons.copy()} Copy all</button>
        <button type="button" class="link" title="Clear the in-memory debug log ring buffer on the server" @click=${onClear}>${icons.trash()} Clear</button>
      </div>
    </div>
    ${renderDataGrid<DebugLogEntry>({
      id: "debug-logs-grid",
      className: "debug-logs-table",
      columns: getDebugColumns(),
      rows: displayRows,
      rowKey: (e) => e.seq,
      emptyMessage: emptyMsg,
      quickActions: () => [
        { action: "copy_msg", title: "Copy message", icon: icons.copy() },
      ],
      menuItems: (e) => [
        { action: "copy_msg", label: "Copy message", shortcut: "C" },
        ...(e.request_id ? [{ action: "copy_rid", label: "Copy request ID" }] : []),
        ...(e.trace_id ? [{ action: "copy_tid", label: "Copy trace ID" }] : []),
      ],
      onAction: (e, action) => {
        switch (action) {
          case "copy_msg":
            void navigator.clipboard?.writeText(e.message);
            showToast(t("grid.toast.copied"));
            return;
          case "copy_rid":
            if (e.request_id) void onCopyCell(e.request_id);
            return;
          case "copy_tid":
            if (e.trace_id) void onCopyCell(e.trace_id);
            return;
        }
      },
      bulkActions: [
        {
          key: "copy_selected",
          label: "Copy selected Markdown",
          run: (picked) => {
            const md = buildMarkdown(picked);
            void copyToClipboardOk(md).then((ok) => {
              if (ok) showToast(`Copied ${picked.length} entries as Markdown`, "success");
            });
          },
        },
      ],
      bulkCountLabel: (n) => t("grid.bulk.selected", { n }),
      __refresh: () => requestUpdate(),
    })}
  `;
}

// Mount the Debug Logs view into `container`: renders the header, filter
// bar and entries table, starts the 2s polling loop, and returns a cleanup
// function. The router calls that cleanup before mounting the next view, so
// the loop does not leak across navigations.
export function mountDebugLogs(container: HTMLElement): () => void {
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
  void pollNow();

  return () => {
    stopped = true;
    if (pollHandle !== null) {
      clearTimeout(pollHandle);
      pollHandle = null;
    }
    cleanupReactive();
  };
}
