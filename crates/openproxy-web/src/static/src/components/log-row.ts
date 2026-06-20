// components/log-row.ts — single row of the live-logs table. Lives
// in its own module so views/logs.js stays focused on orchestration.

import { escapeHtml, escapeAttr } from "../lib/escape.js";
import { formatContext } from "../lib/format.js";
import { STAGE_LABELS } from "../lib/constants.js";
import type { StageEvent, RecentUsageRow } from "../lib/types/api.js";

/** Narrow row shape for `renderLogPhaseHtml`. The live stage can be
 *  missing (request finished before the WS opened) — we accept
 *  `undefined` so the typecheck is happy and render a placeholder. */
export function renderLogPhaseHtml(
  stage: StageEvent | undefined,
  _row: RecentUsageRow,
  total_ms?: number | null,
): string {
  if (!stage) {
    return `<span class="log-phase log-phase--idle" title="No live phase info (request finished before live-log opened)">—</span>`;
  }
  const phase: string = stage.stage || "started";
  const elapsed: number = stage.elapsed_ms || 0;
  const label: string = STAGE_LABELS[phase] || phase;
  const cls: string = `log-phase log-phase--${phase}`;
  let sublabel: string;
  // When the row is finalized (caller passed a `total_ms` and
  // the stage is `completed`/`failed`), the sublabel MUST be
  // the row's `total_ms` (e.g. `"total 4231ms"`) — the live
  // ticker is frozen but the operator should still see the
  // final number. The "stale" branch handles the
  // defense-in-depth case from §4.1: the row is finalized
  // (so the backend wrote it) but the stage is still
  // non-terminal (a slow consumer / lagged subscriber missed
  // the terminal event). We surface that explicitly so the
  // operator knows the row is not actually live anymore.
  if (phase === "completed" || phase === "failed" || phase === "cancelled") {
    sublabel = (total_ms != null && total_ms > 0)
      ? `total ${total_ms}ms`
      : `${elapsed}ms`;
  } else if (total_ms != null && total_ms > 0) {
    sublabel = `${total_ms}ms stale`;
  } else if (phase === "streaming" && stage.ttft_ms != null) sublabel = `ttft ${stage.ttft_ms}ms`;
  else if ((phase === "waiting_ttft" || phase === "streaming") && stage.connect_ms != null) sublabel = `connect ${stage.connect_ms}ms`;
  else sublabel = `${elapsed}ms`;
  return `<span class="${cls}" title="${escapeAttr(label)} (${escapeAttr(sublabel)})">${escapeHtml(label)}<span class="log-phase-sub">${escapeHtml(sublabel)}</span></span>`;
}

// Build the cell <span>s for a log row, in the canonical column
// order, skipping any column the user has hidden via the Columns
// menu. Kept as a small helper so renderLogRowHtml stays readable
// and so the order of the cells always matches LOG_COLUMNS (the
// header is generated from the same constant).
function buildLogRowCells(
  row: RecentUsageRow,
  stage: StageEvent | undefined,
  visibleColumns: Set<string> | null,
  total_ms?: number | null,
): string {
  const cells: string[] = [];
  // `.has(...)` is safe with a missing/null `visibleColumns` (it
  // throws, but views/logs.js always supplies the set after mount).
  // We still guard against null for safety — a missing set means
  // "render everything", which matches the historical default.
  const has: (k: string) => boolean = (k) => !visibleColumns || visibleColumns.has(k);
  if (has("time"))     cells.push(`<span class="log-time">${escapeHtml(row.created_at || "")}</span>`);
  if (has("phase"))    cells.push(renderLogPhaseHtml(stage, row, total_ms));
  if (has("status"))   cells.push(`<span class="log-status">${row.status_code ?? "—"}</span>`);
  if (has("provider")) cells.push(`<span class="log-provider">${escapeHtml(row.provider_id || "")}</span>`);
  if (has("model"))    cells.push(`<span class="log-model">${escapeHtml(row.upstream_model_id || "")}</span>`);
  if (has("tokens"))   cells.push(`<span class="log-tokens">${formatContext(row.prompt_tokens)}↓ ${formatContext(row.completion_tokens)}↑</span>`);
  if (has("latency"))  cells.push(`<span class="log-latency">${row.total_ms || 0}ms</span>`);
  if (has("cost"))     cells.push(`<span class="log-cost">$${(row.cost_usd || 0).toFixed(4)}</span>`);
  return cells.join("");
}

export function renderLogRowHtml(
  row: RecentUsageRow,
  stage: StageEvent | undefined,
  visibleColumns: Set<string> | null,
  total_ms?: number | null,
): string {
  const inflight: boolean = !!(row as any).__inflight;
  const streaming: boolean = !!row.is_streaming && !row.stream_complete;
  const hasError: boolean = !!(row.error_message && row.error_message.length > 0);
  // Inflight rows have status_code === 0 by default — don't treat that
  // as an error. Use a yellow "processing" style instead of red.
  const statusErr: boolean = !inflight && (row.status_code >= 400 || row.status_code === 0 || hasError);
  const cls: string = [
    "log-row",
    inflight ? "processing" : (statusErr ? "error" : "ok"),
    row.race_lost ? "loser" : "",
    streaming ? "streaming" : "",
  ].filter(Boolean).join(" ");
  // `data-trace-id` lets `state/ticker.ts` look up the live stage
  // for this row in `stagesByTraceId` (keyed per-attempt), so
  // retries do not bleed counters over the historical failed row
  // (see the comment on `state.logs.stagesByTraceId` in
  // `state/index.ts`).
  return `
    <button class="${cls}" data-id="${escapeAttr(row.id)}" data-request-id="${escapeAttr(row.request_id || "")}" data-trace-id="${escapeAttr(row.trace_id || "")}" aria-label="Open usage detail for ${escapeAttr(row.request_id || String(row.id) || "")}">
      ${buildLogRowCells(row, stage, visibleColumns, total_ms)}
    </button>
  `;
}
