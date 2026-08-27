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
    const displayMs = liveMs;
    
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

  if (has("type")) {
    const rawKind = (attempt.endpointKind || row?.endpoint_kind || "chat").toLowerCase();
    const path = rawKind === "audio"
      ? "/v1/audio/transcriptions"
      : rawKind === "image"
      ? "/v1/images/generations"
      : rawKind === "embedding"
      ? "/v1/embeddings"
      : rawKind === "video"
      ? "/v1/video/generations"
      : "/v1/chat/completions";
    const icon = rawKind === "audio" ? "🎙️" : rawKind === "image" ? "🎨" : rawKind === "embedding" ? "🧠" : rawKind === "video" ? "🎬" : "💬";
    cells.push(html`<span class="log-type" title="Endpoint: POST ${path} (${rawKind})"><span class="log-type-tag log-type-tag--${rawKind}">${icon} ${rawKind}</span></span>`);
  }
  
  if (has("client")) {
    const isSkipped = attempt.stage === "predict_skipped" || attempt.terminalKind === "predict_skipped";
    const isWinner = isSkipped ? false : (row ? row.client_response : (attempt.terminal ? false : true));
    if (isWinner) {
      cells.push(html`<span class="log-client log-client--winner" title="Response delivered to client (winning attempt)"><svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true"><path d="M3 8.5l3.5 3.5L13 5.5" stroke="currentColor" stroke-width="2" fill="none" stroke-linecap="round" stroke-linejoin="round"/></svg></span>`);
    } else {
      cells.push(html`<span class="log-client log-client--internal" title="Intermediate retry or skipped attempt (not returned to client)"><svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true"><circle cx="8" cy="8" r="5.5" stroke="currentColor" stroke-width="1.5" fill="none" stroke-dasharray="3 2"/></svg></span>`);
    }
  }
  
  if (has("status")) {
    const isSkipped = attempt.stage === "predict_skipped" || attempt.terminalKind === "predict_skipped";
    const statusText = isSkipped ? "skip" : (attempt.statusCode != null && attempt.statusCode > 0 ? String(attempt.statusCode) : "—");
    cells.push(html`<span class="log-status ${isSkipped ? "log-status--skipped" : ""}">${statusText}</span>`);
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
  
  if (has("cache")) {
    if (row && row.cached_tokens != null && row.cached_tokens > 0) {
      cells.push(html`<span class="log-cache" style="color: var(--color-success);" title="${formatContext(row.cached_tokens)} tokens cached by upstream API">🎯 ${formatContext(row.cached_tokens)}</span>`);
    } else {
      cells.push(html`<span class="log-cache">—</span>`);
    }
  }
  
  if (has("compression")) {
    const savings = row ? row.compression_savings_pct : null;
    if (savings != null && savings > 0) {
      const pct = savings < 1 ? savings.toFixed(2) : Math.round(savings).toString();
      const tech = row ? row.compression_techniques : "";
      cells.push(html`<span class="log-compression" style="background: rgba(34, 197, 94, 0.1); padding: 2px 6px; border-radius: 4px; font-weight: 500;" title="Local Compression: ${pct}% savings (BPE cl100k_base) — ${tech}">⚡ ${pct}%</span>`);
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

  const isPredictSkipped = attempt.stage === "predict_skipped" || attempt.terminalKind === "predict_skipped";
  const processing = !attempt.terminal && !isPredictSkipped;
  const isErrorState = !isPredictSkipped && ((attempt.statusCode != null && attempt.statusCode >= 400) || !!attempt.error || attempt.stage === "failed" || attempt.stage === "cancelled");
  const statusErr = !processing && isErrorState;
  const streaming = !attempt.terminal && !isErrorState && !isPredictSkipped && (attempt.row ? (!!attempt.row.is_streaming && !attempt.row.stream_complete) : (attempt.stage === "streaming"));
  
  const cls = [
    "log-row",
    isPredictSkipped ? "predict-skipped" : (processing ? "processing" : (statusErr ? "error" : "ok")),
    (attempt.row?.race_lost || attempt.stage === "cancelled") ? "loser" : "",
    streaming ? "streaming" : "",
  ].filter(Boolean).join(" ");
  
  const cells = buildLogRowCells(attempt, visibleColumns);
  
  const dataId = attempt.rowId || "";
  const dataAttemptKey = attempt.attemptKey || "";
  
  return html`<button class="${cls}" data-id=${dataId} data-attempt-key=${dataAttemptKey} data-request-id=${attempt.requestId || ""} data-trace-id=${attempt.traceId || ""} aria-label="Open usage detail">${cells}</button>`;
}
