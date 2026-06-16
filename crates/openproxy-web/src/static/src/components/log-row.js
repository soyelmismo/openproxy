// components/log-row.js — single row of the live-logs table. Lives
// in its own module so views/logs.js stays focused on orchestration.

import { escapeHtml, escapeAttr } from "../lib/escape.js";
import { formatContext } from "../lib/format.js";
import { STAGE_LABELS } from "../lib/constants.js";

function renderLogPhaseHtml(stage, row) {
  if (!stage) {
    return `<span class="log-phase log-phase--idle" title="No live phase info (request finished before live-log opened)">—</span>`;
  }
  const phase = stage.stage || "started";
  const elapsed = stage.elapsed_ms || 0;
  const label = STAGE_LABELS[phase] || phase;
  const cls = `log-phase log-phase--${phase}`;
  let sublabel;
  if (phase === "streaming" && stage.ttft_ms != null) sublabel = `ttft ${stage.ttft_ms}ms`;
  else if ((phase === "waiting_ttft" || phase === "streaming") && stage.connect_ms != null) sublabel = `connect ${stage.connect_ms}ms`;
  else sublabel = `${elapsed}ms`;
  return `<span class="${cls}" title="${escapeAttr(label)} (${escapeAttr(sublabel)})">${escapeHtml(label)}<span class="log-phase-sub">${escapeHtml(sublabel)}</span></span>`;
}

// Build the cell <span>s for a log row, in the canonical column
// order, skipping any column the user has hidden via the Columns
// menu. Kept as a small helper so renderLogRowHtml stays readable
// and so the order of the cells always matches LOG_COLUMNS (the
// header is generated from the same constant).
function buildLogRowCells(row, stage, visibleColumns) {
  const cells = [];
  // `.has(...)` is safe with a missing/null `visibleColumns` (it
  // throws, but views/logs.js always supplies the set after mount).
  // We still guard against null for safety — a missing set means
  // "render everything", which matches the historical default.
  const has = (k) => !visibleColumns || visibleColumns.has(k);
  if (has("time"))     cells.push(`<span class="log-time">${escapeHtml(row.created_at || "")}</span>`);
  if (has("phase"))    cells.push(renderLogPhaseHtml(stage, row));
  if (has("status"))   cells.push(`<span class="log-status">${row.status_code ?? "—"}</span>`);
  if (has("provider")) cells.push(`<span class="log-provider">${escapeHtml(row.provider_id || "")}</span>`);
  if (has("model"))    cells.push(`<span class="log-model">${escapeHtml(row.upstream_model_id || "")}</span>`);
  if (has("tokens"))   cells.push(`<span class="log-tokens">${formatContext(row.prompt_tokens)}↓ ${formatContext(row.completion_tokens)}↑</span>`);
  if (has("latency"))  cells.push(`<span class="log-latency">${row.total_ms || 0}ms</span>`);
  if (has("cost"))     cells.push(`<span class="log-cost">$${(row.cost_usd || 0).toFixed(4)}</span>`);
  return cells.join("");
}

export function renderLogRowHtml(row, stage, visibleColumns) {
  const streaming = row.is_streaming && !row.stream_complete;
  const cls = [
    "log-row",
    row.status_code >= 400 || row.status_code === 0 ? "error" : "ok",
    row.race_lost ? "loser" : "",
    streaming ? "streaming" : "",
  ].filter(Boolean).join(" ");
  return `
    <button class="${cls}" data-id="${escapeAttr(row.id)}" data-request-id="${escapeAttr(row.request_id || "")}" aria-label="Open usage detail for ${escapeAttr(row.request_id || row.id || "")}">
      ${buildLogRowCells(row, stage, visibleColumns)}
    </button>
  `;
}
