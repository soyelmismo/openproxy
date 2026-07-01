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
  const label: string = STAGE_LABELS[phase] || phase;
  const cls: string = `log-phase log-phase--${phase}`;
  let sublabel: string;
  if (phase === "completed" || phase === "failed" || phase === "cancelled") {
    sublabel = (total_ms != null && total_ms > 0) ? `total ${total_ms}ms` : `${stage.elapsed_ms || 0}ms`;
  } else if (total_ms != null && total_ms > 0) {
    sublabel = `${total_ms}ms stale`;
  } else {
    // LIVE LATENCY: compute elapsed time from the stage event's
    // timestamp. This replaces the old ticker (which modified DOM
    // directly and crashed lit-html). The 250ms render interval in
    // mountLogs refreshes this at 4Hz.
    const t: number = Date.parse(stage.timestamp || "");
    const liveMs: number = isFinite(t) ? Math.max(0, Date.now() - t) : (stage.elapsed_ms || 0);
    // Stale cap: if no new stage event for 60s (non-streaming) or
    // 2s (streaming), freeze the counter so it doesn't climb forever.
    const cap: number = phase === "streaming" ? 2_000 : 60_000;
    const displayMs: number = liveMs > cap ? cap : liveMs;
    if (phase === "streaming" && stage.ttft_ms != null) sublabel = `ttft ${stage.ttft_ms}ms`;
    else if ((phase === "waiting_ttft" || phase === "streaming") && stage.connect_ms != null) sublabel = `connect ${stage.connect_ms}ms`;
    else sublabel = `${displayMs}ms`;
  }
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
  if (has("client")) {
    // Client response indicator: shows whether this row's response
    // was actually delivered to the HTTP client (winner) or was an
    // intermediate retry that never reached the client.
    if (row.client_response) {
      cells.push(html`<span class="log-client log-client--winner" title="Response delivered to client (winning attempt)"><svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true"><path d="M3 8.5l3.5 3.5L13 5.5" stroke="currentColor" stroke-width="2" fill="none" stroke-linecap="round" stroke-linejoin="round"/></svg></span>`);
    } else {
      cells.push(html`<span class="log-client log-client--internal" title="Intermediate retry (not returned to client)"><svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true"><circle cx="8" cy="8" r="5.5" stroke="currentColor" stroke-width="1.5" fill="none" stroke-dasharray="3 2"/></svg></span>`);
    }
  }
  if (has("status"))   cells.push(html`<span class="log-status">${row.status_code ?? "—"}</span>`);
  if (has("provider")) cells.push(html`<span class="log-provider">${row.provider_id || ""}</span>`);
  if (has("model"))    cells.push(html`<span class="log-model">${row.upstream_model_id || ""}</span>`);
  if (has("tokens")) {
    const ptEst = row.prompt_tokens_estimated ? "≈" : "";
    const ctEst = row.completion_tokens_estimated ? "≈" : "";
    const title = (row.prompt_tokens_estimated || row.completion_tokens_estimated)
      ? "Tokens marked ≈ are estimated (upstream didn't report usage)"
      : "Tokens reported by upstream";
    cells.push(html`<span class="log-tokens" title="${title}">${ptEst}${formatContext(row.prompt_tokens)}↓ ${ctEst}${formatContext(row.completion_tokens)}↑</span>`);
  }
  if (has("latency"))  {
    // Live latency for inflight rows: compute from stage timestamp.
    // For finalized rows, use the DB total_ms.
    let latencyMs: number = row.total_ms || 0;
    if (latencyMs === 0 && stage && stage.timestamp) {
      const t = Date.parse(stage.timestamp);
      if (isFinite(t)) {
        const live = Date.now() - t;
        if (live > 0) latencyMs = live;
      }
    }
    cells.push(html`<span class="log-latency">${latencyMs}ms</span>`);
  }
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
