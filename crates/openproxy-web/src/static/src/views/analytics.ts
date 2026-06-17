// views/analytics.ts — analytics dashboards: summary, latency
// percentiles, race stats, by-model breakdown. Pulls from
// /usage/summary, /usage/by-model, /usage/latency, /usage/races.

import { api } from "../state/api.js";
import { escapeHtml } from "../lib/escape.js";
import { pageHeader } from "../components/page-header.js";
import { card } from "../components/card.js";
import type { ByModelRow, UsageSummary } from "../lib/types/api.js";

// The two endpoints that don't have a dedicated type in
// lib/types/api.ts (the by-model row already has one). Both
// payloads are flat numeric objects — modelled as a `Record` with
// optional number fields so the format-fallback branches in the
// renderer can read them as `null` when the upstream omitted them.
interface LatencyPayload {
  samples?: number;
  p50_connect_ms?: number | null;
  p95_connect_ms?: number | null;
  p50_ttft_ms?: number | null;
  p95_ttft_ms?: number | null;
  p50_total_ms?: number | null;
  p95_total_ms?: number | null;
}
interface RaceStatsPayload {
  total_races?: number;
  winners?: number;
  losers?: number;
}

function fmtMs(v: number | null | undefined): string {
  return v == null ? "—" : `${v.toFixed(0)} ms`;
}

export async function mountAnalytics(): Promise<void> {
  const main = document.getElementById("main");
  if (!main) return;
  main.innerHTML = pageHeader({ title: "Analytics" }) + `<div class="loading">Loading...</div>`;
  try {
    const [summary, byModel, latency, races] = await Promise.all([
      api("/usage/summary") as Promise<UsageSummary>,
      api("/usage/by-model") as Promise<ByModelRow[]>,
      api("/usage/latency") as Promise<LatencyPayload>,
      api("/usage/races") as Promise<RaceStatsPayload>,
    ]);
    const summaryBlock = card("Summary", `
      <div class="metrics">
        <div><label>Unique requests</label><div class="value">${summary.unique_requests}</div></div>
        <div><label>Total rows</label><div class="value">${summary.total_rows}</div></div>
        <div><label>Winners</label><div class="value">${summary.winners}</div></div>
        <div><label>Losers</label><div class="value">${summary.losers}</div></div>
        <div><label>Errors</label><div class="value">${summary.errors}</div></div>
        <div><label>Prompt tokens</label><div class="value">${summary.total_prompt_tokens}</div></div>
        <div><label>Completion tokens</label><div class="value">${summary.total_completion_tokens}</div></div>
        <div><label>Total cost USD</label><div class="value">$${summary.total_cost_usd.toFixed(4)}</div></div>
        <div><label>Avg TTFT ms</label><div class="value">${summary.avg_ttft_ms ? summary.avg_ttft_ms.toFixed(1) : "—"}</div></div>
      </div>
    `);
    const latencyBlock = card("Latency percentiles (winners only)", `
      <div class="metrics">
        <div><label>Samples</label><div class="value">${latency.samples}</div></div>
        <div><label>p50 connect ms</label><div class="value">${fmtMs(latency.p50_connect_ms)}</div></div>
        <div><label>p95 connect ms</label><div class="value">${fmtMs(latency.p95_connect_ms)}</div></div>
        <div><label>p50 TTFT ms</label><div class="value">${fmtMs(latency.p50_ttft_ms)}</div></div>
        <div><label>p95 TTFT ms</label><div class="value">${fmtMs(latency.p95_ttft_ms)}</div></div>
        <div><label>p50 total ms</label><div class="value">${fmtMs(latency.p50_total_ms)}</div></div>
        <div><label>p95 total ms</label><div class="value">${fmtMs(latency.p95_total_ms)}</div></div>
      </div>
    `);
    const raceBlock = card("Race stats", `
      <div class="metrics">
        <div><label>Total races</label><div class="value">${races.total_races}</div></div>
        <div><label>Winners</label><div class="value">${races.winners}</div></div>
        <div><label>Losers</label><div class="value">${races.losers}</div></div>
      </div>
    `);
    const byModelRows = byModel.map((r) =>
      `<tr><td>${escapeHtml(r.provider_id)}</td><td>${escapeHtml(r.upstream_model_id)}</td><td>${r.unique_requests}</td><td>${r.total_rows}</td><td>$${r.total_cost_usd.toFixed(4)}</td></tr>`
    ).join("");
    const byModelBlock = card("By model", `
      <table>
        <thead><tr><th>Provider</th><th>Model</th><th>Unique</th><th>Total</th><th>Cost USD</th></tr></thead>
        <tbody>${byModelRows}</tbody>
      </table>
    `);
    main.innerHTML = pageHeader({ title: "Analytics" }) + summaryBlock + latencyBlock + raceBlock + byModelBlock;
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    main.innerHTML = pageHeader({ title: "Analytics" }) +
      `<div class="banner banner-error">${escapeHtml(msg)}</div>`;
  }
}
