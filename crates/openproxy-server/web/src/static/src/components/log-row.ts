// components/log-row.ts — single row of the live-logs table.
// Migrated to lit-html: returns TemplateResult instead of string.

import { html, type TemplateResult } from 'lit-html';
import { formatContext } from "../lib/format.js";
import { STAGE_LABELS } from "../lib/constants.js";
import type { StageEvent, RecentUsageRow } from "../lib/types/api.js";

export function renderLogPhaseHtml(
  stage: StageEvent | undefined,
  _row: RecentUsageRow,
  total_ms?: number | null,
): TemplateResult {
  if (!stage) {
    return html`<span class="log-phase log-phase--idle" title="No live phase info (request finished before live-log opened)">—</span>`;
  }
  const phase: string = stage.stage || "started";
  const elapsed: number = stage.elapsed_ms || 0;
  const label: string = STAGE_LABELS[phase] || phase;
  const cls: string = `log-phase log-phase--${phase}`;
  let sublabel: string;
  if (phase === "completed" || phase === "failed" || phase === "cancelled") {
    sublabel = (total_ms != null && total_ms > 0) ? `total ${total_ms}ms` : `${elapsed}ms`;
  } else if (total_ms != null && total_ms > 0) {
    sublabel = `${total_ms}ms stale`;
  } else if (phase === "streaming" && stage.ttft_ms != null) sublabel = `ttft ${stage.ttft_ms}ms`;
  else if ((phase === "waiting_ttft" || phase === "streaming") && stage.connect_ms != null) sublabel = `connect ${stage.connect_ms}ms`;
  else sublabel = `${elapsed}ms`;
  return html`<span class="${cls}" title="${label} (${sublabel})">${label}<span class="log-phase-sub">${sublabel}</span></span>`;
}

function buildLogRowCells(
  row: RecentUsageRow,
  stage: StageEvent | undefined,
  visibleColumns: Set<string> | null,
  total_ms?: number | null,
): TemplateResult[] {
  const cells: TemplateResult[] = [];
  const has = (k: string): boolean => !visibleColumns || visibleColumns.has(k);
  if (has("time"))     cells.push(html`<span class="log-time">${row.created_at || ""}</span>`);
  if (has("phase"))    cells.push(renderLogPhaseHtml(stage, row, total_ms));
  if (has("status"))   cells.push(html`<span class="log-status">${row.status_code ?? "—"}</span>`);
  if (has("provider")) cells.push(html`<span class="log-provider">${row.provider_id || ""}</span>`);
  if (has("model"))    cells.push(html`<span class="log-model">${row.upstream_model_id || ""}</span>`);
  if (has("tokens"))   cells.push(html`<span class="log-tokens">${formatContext(row.prompt_tokens)}↓ ${formatContext(row.completion_tokens)}↑</span>`);
  if (has("latency"))  cells.push(html`<span class="log-latency">${row.total_ms || 0}ms</span>`);
  if (has("cost"))     cells.push(html`<span class="log-cost">$${(row.cost_usd || 0).toFixed(4)}</span>`);
  if (has("compression")) {
    const savings = row.compression_savings_pct ?? stage?.compression_savings_pct ?? null;
    if (savings != null && savings > 0) {
      const pct = Math.round(savings);
      const tech = row.compression_techniques ?? stage?.compression_techniques ?? "";
      cells.push(html`<span class="log-compression" title="Compressed: ${pct}% saved — ${tech}">-${pct}%</span>`);
    } else {
      cells.push(html`<span class="log-compression log-compression--none" title="No compression applied (or mode is Off)">—</span>`);
    }
  }
  return cells;
}

export function renderLogRowHtml(
  row: RecentUsageRow,
  stage: StageEvent | undefined,
  visibleColumns: Set<string> | null,
  total_ms?: number | null,
): TemplateResult {
  const inflight: boolean = !!(row as unknown as Record<string, unknown>)["__inflight"];
  const streaming: boolean = !!row.is_streaming && !row.stream_complete;
  const hasError: boolean = !!(row.error_message && row.error_message.length > 0);
  const statusErr: boolean = !inflight && (row.status_code >= 400 || row.status_code === 0 || hasError);
  const cls: string = [
    "log-row",
    inflight ? "processing" : (statusErr ? "error" : "ok"),
    row.race_lost ? "loser" : "",
    streaming ? "streaming" : "",
  ].filter(Boolean).join(" ");
  const cells = buildLogRowCells(row, stage, visibleColumns, total_ms);
  return html`<button class="${cls}" data-id="${String(row.id)}" data-request-id="${row.request_id || ""}" data-trace-id="${row.trace_id || ""}" aria-label="Open usage detail for ${row.request_id || String(row.id) || ""}">${cells}</button>`;
}
