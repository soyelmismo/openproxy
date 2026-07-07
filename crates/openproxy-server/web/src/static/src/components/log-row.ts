// components/log-row.ts — single row of the live-logs table.
// Refactored to accept AttemptState from live-logs-store and render declaratively.

import { html, type TemplateResult } from 'lit-html';
import { formatContext } from "../lib/format.js";
import { STAGE_LABELS } from "../lib/constants.js";
import type { AttemptState } from "../state/live-logs-store.js";

export function renderLogPhaseHtml(attempt: AttemptState): TemplateResult {
  const phase = attempt.stage || "started";
  const label = STAGE_LABELS[phase] || phase;
  const cls = `log-phase log-phase--${phase}`;
  
  let sublabel = "";
  if (attempt.terminal) {
    sublabel = attempt.row ? `total ${attempt.elapsedMsAtEvent}ms` : `${attempt.elapsedMsAtEvent}ms`;
  } else {
    const liveMs = attempt.elapsedMsAtEvent;
    // Stale cap
    const cap = phase === "streaming" ? 2_000 : 60_000;
    const displayMs = liveMs > cap ? cap : liveMs;
    
    if (phase === "streaming" && attempt.ttftMs != null) {
      sublabel = `ttft ${attempt.ttftMs}ms`;
    } else if ((phase === "waiting_ttft" || phase === "streaming") && attempt.connectMs != null) {
      sublabel = `connect ${attempt.connectMs}ms`;
    } else {
      sublabel = `${displayMs}ms`;
    }
  }

  return html`<span class="${cls}" title="${label} (${sublabel})">${label}<span class="log-phase-sub">${sublabel}</span></span>`;
}

function buildLogRowCells(
  attempt: AttemptState,
  visibleColumns: Set<string> | null
): TemplateResult[] {
  const cells: TemplateResult[] = [];
  const has = (k: string): boolean => !visibleColumns || visibleColumns.has(k);
  const row = attempt.row;

  if (has("time")) {
    const timeStr = row ? (row.created_at || "") : new Date(attempt.startedAtMs).toISOString();
    cells.push(html`<span class="log-time">${timeStr}</span>`);
  }
  
  if (has("phase")) {
    cells.push(renderLogPhaseHtml(attempt));
  }
  
  if (has("client")) {
    const isWinner = row ? row.client_response : (attempt.terminal ? false : true); // Assume winner if inflight
    if (isWinner) {
      cells.push(html`<span class="log-client log-client--winner" title="Response delivered to client (winning attempt)"><svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true"><path d="M3 8.5l3.5 3.5L13 5.5" stroke="currentColor" stroke-width="2" fill="none" stroke-linecap="round" stroke-linejoin="round"/></svg></span>`);
    } else {
      cells.push(html`<span class="log-client log-client--internal" title="Intermediate retry (not returned to client)"><svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true"><circle cx="8" cy="8" r="5.5" stroke="currentColor" stroke-width="1.5" fill="none" stroke-dasharray="3 2"/></svg></span>`);
    }
  }
  
  if (has("status")) {
    cells.push(html`<span class="log-status">${attempt.statusCode ?? "—"}</span>`);
  }
  
  if (has("provider")) {
    cells.push(html`<span class="log-provider">${attempt.providerId || ""}</span>`);
  }
  
  if (has("model")) {
    cells.push(html`<span class="log-model">${attempt.upstreamModelId || ""}</span>`);
  }
  
  if (has("tokens")) {
    if (row) {
      const ptEst = row.prompt_tokens_estimated ? "≈" : "";
      const ctEst = row.completion_tokens_estimated ? "≈" : "";
      const title = (row.prompt_tokens_estimated || row.completion_tokens_estimated)
        ? "Tokens marked ≈ are estimated (upstream didn't report usage)"
        : "Tokens reported by upstream";
      cells.push(html`<span class="log-tokens" title="${title}">${ptEst}${formatContext(row.prompt_tokens)}↓ ${ctEst}${formatContext(row.completion_tokens)}↑</span>`);
    } else {
      cells.push(html`<span class="log-tokens">—</span>`);
    }
  }
  
  if (has("latency")) {
    cells.push(html`<span class="log-latency">${attempt.elapsedMsAtEvent}ms</span>`);
  }
  
  if (has("cost")) {
    if (row) {
      cells.push(html`<span class="log-cost">$${(row.cost_usd || 0).toFixed(4)}</span>`);
    } else {
      cells.push(html`<span class="log-cost">—</span>`);
    }
  }
  
  if (has("compression")) {
    const savings = row ? row.compression_savings_pct : null;
    if (savings != null && savings > 0) {
      const pct = Math.round(savings);
      const tech = row ? row.compression_techniques : "";
      cells.push(html`<span class="log-compression" title="Token savings: ${pct}% (BPE cl100k_base) — ${tech}">-${pct}%</span>`);
    } else {
      cells.push(html`<span class="log-compression log-compression--none" title="No compression applied (or mode is Off)">—</span>`);
    }
  }
  
  return cells;
}

export function renderLogRowHtml(
  attempt: AttemptState,
  visibleColumns: Set<string> | null,
  nowMs: number
): TemplateResult {
  // Update live latency if not terminal
  if (!attempt.terminal) {
    attempt.elapsedMsAtEvent = Math.max(0, nowMs - attempt.startedAtMs);
  }

  const processing = !attempt.terminal;
  const isErrorState = (attempt.statusCode && attempt.statusCode >= 400) || attempt.statusCode === 0 || !!attempt.error || attempt.stage === "failed" || attempt.stage === "cancelled";
  const statusErr = !processing && isErrorState;
  const streaming = attempt.row ? (!!attempt.row.is_streaming && !attempt.row.stream_complete) : (attempt.stage === "streaming");
  
  const cls = [
    "log-row",
    processing ? "processing" : (statusErr ? "error" : "ok"),
    (attempt.row?.race_lost || attempt.stage === "cancelled") ? "loser" : "",
    streaming ? "streaming" : "",
  ].filter(Boolean).join(" ");
  
  const cells = buildLogRowCells(attempt, visibleColumns);
  
  const identityAttr = attempt.rowId ? `data-id="${attempt.rowId}"` : `data-attempt-key="${attempt.attemptKey}"`;
  
  return html`<button class="${cls}" ${identityAttr} data-request-id="${attempt.requestId || ""}" data-trace-id="${attempt.traceId || ""}" aria-label="Open usage detail">${cells}</button>`;
}
