// views/key-usage.ts — Per-key usage recap over /keys/:id/usage and
// /usage/by-day?api_key_id=... . Shows a KPI summary row (requests /
// cost / error rate / avg latency / last used), a daily-usage chart
// reusing the analytics dailyUsageChart, and a compact dense metrics grid.
// No `innerHTML` is assigned: metrics live in module-local state and
// `requestUpdate()` drives lit-html's diffing.

import { html, type TemplateResult } from 'lit-html';
import { unsafeHTML } from 'lit-html/directives/unsafe-html.js';
import { api } from "../state/api.js";
import { createView } from "../lib/view-utils.js";
import { dailyUsageChart } from "../components/charts.js";
import type { ByDayRow, UsageSummary } from "../lib/types/api.js";

// Payload from /keys/:id/usage: a small per-key headline object plus the
// full `usage::summary` roll-up carrying avg_ttft_ms / avg_total_ms for the
// latency KPI. Both sub-objects stay optional so a partial response does not
// crash the render.
interface KeyUsageHead {
  key?: {
    total_rows?: number;
    unique_requests?: number;
    errors?: number;
    total_cost_usd?: number;
    last_used_at?: string | null;
  } | null;
  summary?: UsageSummary | null;
}

let keyId: number = 0;
let head: KeyUsageHead | null = null;
let byDay: ByDayRow[] | null = null;
let loadError: string | null = null;

// Formatting matches the analytics dashboard: cost at 4dp, latency in
// ms with a `—` fallback for nulls, error rate as a percentage at 1dp.
function fmtCost(v: number): string {
  return `$${v.toFixed(4)}`;
}

function fmtMs(v: number | null | undefined): string {
  if (v == null) return "—";
  return `${v.toFixed(0)} ms`;
}

function fmtPct(num: number, denom: number): string {
  if (!denom) return "0.0%";
  return `${((num / denom) * 100).toFixed(1)}%`;
}

function renderKpiTile(label: string, value: string, valueClass = ""): TemplateResult {
  return html`<div class="kpi-tile">
    <div class="kpi-label">${label}</div>
    <div class="kpi-value ${valueClass}">${value}</div>
  </div>`;
}

function renderChartBlock(): TemplateResult {
  if (byDay === null) {
    return html`<section class="card chart-card">
      <div class="card-title">Daily usage</div>
      <div class="card-body"><div class="loading">Loading...</div></div>
    </section>`;
  }
  return html`<section class="card chart-card">
    <div class="card-title">Daily usage</div>
    <div class="card-body">${unsafeHTML(dailyUsageChart(byDay))}</div>
  </section>`;
}

function renderKeyUsage(): TemplateResult {
  if (loadError) {
    return html`
      <div class="page-header"><a href="#/keys" class="back-link">← All keys</a><h2>API key #${keyId}</h2></div>
      <div class="banner banner-error">${loadError}</div>
    `;
  }
  if (!head) {
    return html`
      <div class="page-header"><a href="#/keys" class="back-link">← All keys</a><h2>API key #${keyId}</h2></div>
      <div class="loading">Loading...</div>
    `;
  }

  const k = head.key ?? {};
  const s: Partial<UsageSummary> = head.summary ?? {};
  const unique: number = k.unique_requests ?? s.unique_requests ?? 0;
  const total: number = k.total_rows ?? s.total_rows ?? 0;
  const errors: number = k.errors ?? s.errors ?? 0;
  const cost: number = k.total_cost_usd ?? s.total_cost_usd ?? 0;
  const avgLatency: number | null = s.avg_total_ms ?? null;
  const avgTtft: number | null = s.avg_ttft_ms ?? null;
  const last: string = k.last_used_at ?? "never";
  const promptTok: number = s.total_prompt_tokens ?? 0;
  const completionTok: number = s.total_completion_tokens ?? 0;
  const winners: number = s.winners ?? 0;
  const losers: number = s.losers ?? 0;

  const errorRatePct = total > 0 ? (errors / total) * 100 : 0;

  const kpiRow = html`<div class="home-kpi-row">
    ${renderKpiTile("Total requests", String(unique))}
    ${renderKpiTile("Total cost", fmtCost(cost))}
    ${renderKpiTile("Error rate", `${errorRatePct.toFixed(1)}%`, errorRatePct > 5 ? "kpi-trend-down" : "")}
    ${renderKpiTile("Avg latency", fmtMs(avgLatency))}
  </div>`;

  return html`
    <div class="page-header"><a href="#/keys" class="back-link">← All keys</a><h2>API key #${keyId} usage</h2></div>
    ${kpiRow}
    ${renderChartBlock()}
    <section class="detail-section key-usage-dense-card">
      <div class="section-header"><h3>Headline metrics</h3></div>
      <div class="key-usage-dense-grid">
        <div class="key-usage-dense-item">
          <span class="dense-label">Total rows</span>
          <span class="dense-value">${total}</span>
        </div>
        <div class="key-usage-dense-item">
          <span class="dense-label">Unique requests</span>
          <span class="dense-value">${unique}</span>
        </div>
        <div class="key-usage-dense-item">
          <span class="dense-label">Winners</span>
          <span class="dense-value">${winners}</span>
        </div>
        <div class="key-usage-dense-item">
          <span class="dense-label">Losers</span>
          <span class="dense-value">${losers}</span>
        </div>
        <div class="key-usage-dense-item">
          <span class="dense-label">Errors (4xx/5xx)</span>
          <span class="dense-value">${errors} (${fmtPct(errors, total)})</span>
        </div>
        <div class="key-usage-dense-item">
          <span class="dense-label">Total cost (USD)</span>
          <span class="dense-value">${fmtCost(cost)}</span>
        </div>
        <div class="key-usage-dense-item">
          <span class="dense-label">Prompt tokens</span>
          <span class="dense-value">${promptTok}</span>
        </div>
        <div class="key-usage-dense-item">
          <span class="dense-label">Completion tokens</span>
          <span class="dense-value">${completionTok}</span>
        </div>
        <div class="key-usage-dense-item">
          <span class="dense-label">Avg TTFT</span>
          <span class="dense-value">${fmtMs(avgTtft)}</span>
        </div>
        <div class="key-usage-dense-item">
          <span class="dense-label">Avg total latency</span>
          <span class="dense-value">${fmtMs(avgLatency)}</span>
        </div>
        <div class="key-usage-dense-item">
          <span class="dense-label">Last used</span>
          <span class="dense-value">${last}</span>
        </div>
      </div>
    </section>
    <p style="margin-top:1.5rem;text-align:center;"><a href="#/analytics?api_key_id=${keyId}" class="btn secondary">📊 View detailed breakdowns in Analytics →</a></p>
  `;
}

export async function mountKeyUsage(
  id: number,
  target?: HTMLElement | null,
): Promise<(() => void) | void> {
  const container = target ?? document.getElementById("main");
  keyId = id;
  head = null;
  byDay = null;
  loadError = null;
  return createView(
    renderKeyUsage,
    async () => {
      const [headResp, byDayResp] = await Promise.all([
        api(`/keys/${id}/usage`) as Promise<KeyUsageHead>,
        api(`/usage/by-day?api_key_id=${id}`).catch((): ByDayRow[] => []) as Promise<ByDayRow[]>,
      ]);
      head = headResp;
      byDay = byDayResp;
    },
    (msg) => { loadError = msg; },
    container,
  );
}
