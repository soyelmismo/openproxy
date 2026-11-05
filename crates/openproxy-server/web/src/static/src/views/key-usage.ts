// Per-key usage recap over /keys/:id/usage and
// /usage/by-day?api_key_id=... . Shows a KPI summary row (requests /
// cost / error rate / avg latency / last used), a daily-usage chart
// reusing the analytics dailyUsageChart, and a metrics table. No
// `innerHTML` is assigned: metrics live in module-local state and
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

// `loadError` swaps the loading view for an inline banner when the
// headline fetch fails. `byDay` stays null until its fetch resolves so the
// chart shows its own placeholder independently of the headline metrics.
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

// `valueClass` colours a high error rate red, like the home dashboard.
function renderKpiTile(label: string, value: string, valueClass = ""): TemplateResult {
  return html`<div class="kpi-tile">
    <div class="kpi-label">${label}</div>
    <div class="kpi-value ${valueClass}">${value}</div>
  </div>`;
}

// The chart renders only once `byDay` resolves (even if empty); before
// that a `Loading...` placeholder shows. An empty array lets
// `dailyUsageChart` render its own "No data for the selected range." message.
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

  // `key` is the per-key headline row; `summary` is the full roll-up with
  // avg_ttft_ms / avg_total_ms / winners / losers / token counts. Both fall
  // back to 0 so the tiles render "0" instead of calling `.toFixed()` on
  // `undefined`.
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

  // Error rate — used both for the KPI tile and for the table row.
  const errorRatePct = total > 0 ? (errors / total) * 100 : 0;

  // KPI row: requests / cost / error rate / avg latency. The error
  // rate tile turns red when it exceeds 5%, matching the home
  // dashboard's threshold.
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
    <section class="detail-section">
      <div class="section-header"><h3>Headline metrics</h3></div>
      <div class="table-wrap">
        <table class="key-usage-table responsive-kv-table">
          <tbody>
            <tr><th>Total rows</th><td>${total}</td></tr>
            <tr><th>Unique requests</th><td>${unique}</td></tr>
            <tr><th>Winners</th><td>${winners}</td></tr>
            <tr><th>Losers</th><td>${losers}</td></tr>
            <tr><th>Errors (4xx/5xx)</th><td>${errors} (${fmtPct(errors, total)})</td></tr>
            <tr><th>Total cost (USD)</th><td>${fmtCost(cost)}</td></tr>
            <tr><th>Prompt tokens</th><td>${promptTok}</td></tr>
            <tr><th>Completion tokens</th><td>${completionTok}</td></tr>
            <tr><th>Avg TTFT</th><td>${fmtMs(avgTtft)}</td></tr>
            <tr><th>Avg total latency</th><td>${fmtMs(avgLatency)}</td></tr>
            <tr><th>Last used</th><td>${last}</td></tr>
          </tbody>
        </table>
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
